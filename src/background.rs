//! Asynchronous, bounded background-image loading and painting for egui.
//!
//! [`BackgroundRenderer`] owns a single loader thread. Filesystem access, HTTPS downloads,
//! image decoding, resizing, and blur all happen on that thread; the UI thread only polls
//! completions and uploads the resulting [`egui::ColorImage`] as a texture.
//!
//! Pass the selected global or per-page [`BackgroundSettings`] to
//! [`BackgroundRenderer::paint`]. [`BackgroundSource::None`] disables both the image and
//! overlay, preserving the launcher's existing solid background.

use crate::ui_settings::{
    BackgroundFit, BackgroundSettings, BackgroundSource, HorizontalAlignment, VerticalAlignment,
};
use eframe::egui;
use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::time::Duration;

const MAX_ENCODED_BYTES: usize = 20 * 1024 * 1024;
const MAX_DIMENSION: u32 = 4096;
const MAX_PIXELS: u64 = 4096 * 4096;
const MAX_DECODER_ALLOC: u64 = 96 * 1024 * 1024;
const MAX_BLUR_RADIUS: f32 = 100.0;
const MAX_REDIRECTS: usize = 5;
const MAX_FOLDER_IMAGES: usize = 10_000;
const MAX_TILES: usize = 4096;

/// Observable state of the current background request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackgroundStatus {
    /// No configuration has been observed yet.
    Idle,
    /// Background images are disabled. Painting is a no-op.
    Disabled,
    /// The current source is being read, decoded, and processed.
    Loading,
    /// The current image is uploaded and available for painting.
    Ready,
    /// The current source or configuration could not be loaded.
    Failed(String),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Fit {
    #[default]
    Cover,
    Contain,
    Stretch,
    Tile,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Horizontal {
    Left,
    #[default]
    Center,
    Right,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Vertical {
    Top,
    #[default]
    Center,
    Bottom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Source {
    File(PathBuf),
    Url(String),
    Folder(PathBuf),
}

#[derive(Clone)]
struct PaintConfig {
    enabled: bool,
    source: Option<Source>,
    fit: Fit,
    horizontal: Horizontal,
    vertical: Vertical,
    opacity: f32,
    blur: f32,
    overlay: egui::Color32,
    overlay_opacity: f32,
}

impl Default for PaintConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source: None,
            fit: Fit::Cover,
            horizontal: Horizontal::Center,
            vertical: Vertical::Center,
            opacity: 1.0,
            blur: 0.0,
            overlay: egui::Color32::BLACK,
            overlay_opacity: 0.0,
        }
    }
}

struct Job {
    generation: u64,
    selection_seed: u64,
    source: Source,
    blur: f32,
    repaint: egui::Context,
}

struct Completion {
    generation: u64,
    result: Result<egui::ColorImage, String>,
}

/// UI-owned controller for one asynchronously loaded background image.
///
/// Keep this value in `app::Ferrite` and call [`paint`](Self::paint) every frame before
/// foreground UI. Changing the source, blur, persisted reload nonce, or `page_key` starts
/// a new generation. A completion from an older generation is always discarded.
pub struct BackgroundRenderer {
    jobs: SyncSender<Job>,
    completions: Receiver<Completion>,
    identity: Option<u64>,
    generation: u64,
    submitted_generation: Option<u64>,
    config: PaintConfig,
    texture: Option<egui::TextureHandle>,
    status: BackgroundStatus,
}

impl Default for BackgroundRenderer {
    fn default() -> Self {
        let (jobs, job_receiver) = mpsc::sync_channel::<Job>(1);
        let (completion_sender, completions) = mpsc::sync_channel::<Completion>(1);
        let _ = std::thread::Builder::new()
            .name("background-loader".to_owned())
            .spawn(move || worker(job_receiver, completion_sender));
        Self {
            jobs,
            completions,
            identity: None,
            generation: 0,
            submitted_generation: None,
            config: PaintConfig::default(),
            texture: None,
            status: BackgroundStatus::Idle,
        }
    }
}

impl BackgroundRenderer {
    /// Returns the state associated with the latest config/page generation.
    pub fn status(&self) -> &BackgroundStatus {
        &self.status
    }

    /// Synchronizes, polls, and paints the configured background into `rect`.
    ///
    /// `page_key` should identify the current top-level page. Its value is only hashed and
    /// is never sent to the worker. A disabled/default config paints neither image nor
    /// overlay. Invalid or empty rectangles are ignored.
    pub fn paint<K: Hash>(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        rect: egui::Rect,
        config: &BackgroundSettings,
        page_key: &K,
    ) {
        self.synchronize(config, page_key);
        self.poll(ctx);
        self.submit_if_needed(ctx);

        if !self.config.enabled || rect.width() <= 0.0 || rect.height() <= 0.0 {
            return;
        }
        if let Some(texture) = &self.texture {
            paint_texture(painter, rect, texture, &self.config);
        }
        let overlay_alpha = normalized_alpha(self.config.overlay.a(), self.config.overlay_opacity);
        if overlay_alpha != 0 {
            let overlay = egui::Color32::from_rgba_unmultiplied(
                self.config.overlay.r(),
                self.config.overlay.g(),
                self.config.overlay.b(),
                overlay_alpha,
            );
            painter.rect_filled(rect, 0.0, overlay);
        }
        if matches!(self.status, BackgroundStatus::Loading) {
            ctx.request_repaint_after(Duration::from_millis(100));
        }
    }

    fn synchronize<K: Hash>(&mut self, config: &BackgroundSettings, page_key: &K) {
        let identity = load_identity(config, page_key);
        let next_config = PaintConfig::from(config);
        if self.identity == Some(identity) {
            self.config = next_config;
            return;
        }

        self.identity = Some(identity);
        self.generation = self.generation.wrapping_add(1);
        self.submitted_generation = None;
        self.texture = None;
        self.config = next_config;
        self.status = if self.config.enabled {
            BackgroundStatus::Loading
        } else {
            BackgroundStatus::Disabled
        };
    }

    fn poll(&mut self, ctx: &egui::Context) {
        while let Ok(completion) = self.completions.try_recv() {
            if completion.generation != self.generation || !self.config.enabled {
                continue;
            }
            match completion.result {
                Ok(image) => {
                    self.texture = Some(ctx.load_texture(
                        "launcher-background",
                        image,
                        egui::TextureOptions::LINEAR,
                    ));
                    self.status = BackgroundStatus::Ready;
                }
                Err(error) => self.fail(error),
            }
        }
    }

    fn submit_if_needed(&mut self, ctx: &egui::Context) {
        if !self.config.enabled
            || self.config.source.is_none()
            || self.submitted_generation == Some(self.generation)
            || matches!(self.status, BackgroundStatus::Failed(_))
        {
            return;
        }
        let job = Job {
            generation: self.generation,
            selection_seed: self.identity.unwrap_or_default(),
            source: self.config.source.clone().expect("source checked above"),
            blur: self.config.blur,
            repaint: ctx.clone(),
        };
        match self.jobs.try_send(job) {
            Ok(()) => self.submitted_generation = Some(self.generation),
            Err(TrySendError::Full(_)) => ctx.request_repaint_after(Duration::from_millis(50)),
            Err(TrySendError::Disconnected(_)) => {
                self.fail("background loader is unavailable".to_owned())
            }
        }
    }

    fn fail(&mut self, error: String) {
        self.texture = None;
        self.status = BackgroundStatus::Failed(error);
    }
}

fn worker(jobs: Receiver<Job>, completions: SyncSender<Completion>) {
    while let Ok(job) = jobs.recv() {
        let generation = job.generation;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| load_job(&job)))
            .unwrap_or_else(|_| Err("background image decoder panicked".to_owned()));
        if completions.send(Completion { generation, result }).is_err() {
            return;
        }
        job.repaint.request_repaint();
    }
}

fn load_job(job: &Job) -> Result<egui::ColorImage, String> {
    let source = match &job.source {
        Source::Folder(folder) => Source::File(choose_folder_image(folder, job.selection_seed)?),
        source => source.clone(),
    };
    let bytes = match source {
        Source::File(path) => read_bounded(&path)?,
        Source::Url(url) => download_bounded(&url)?,
        Source::Folder(_) => unreachable!("folder source was resolved above"),
    };
    decode(bytes, job.blur)
}

fn read_bounded(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        File::open(path).map_err(|error| format!("could not open background image: {error}"))?;
    if file
        .metadata()
        .ok()
        .is_some_and(|metadata| metadata.len() > MAX_ENCODED_BYTES as u64)
    {
        return Err("background image exceeds the encoded size limit".to_owned());
    }
    read_limited(file)
}

fn download_bounded(url: &str) -> Result<Vec<u8>, String> {
    let url = reqwest::Url::parse(url).map_err(|_| "background URL is invalid".to_owned())?;
    if url.scheme() != "https" {
        return Err("background URL must use HTTPS".to_owned());
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("ferrite-launcher/", env!("CARGO_PKG_VERSION")))
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(20))
        .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
        .build()
        .map_err(|error| format!("could not initialize background download: {error}"))?;
    let response = client
        .get(url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .map_err(|error| format!("could not download background image: {error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MAX_ENCODED_BYTES as u64)
    {
        return Err("background image exceeds the encoded size limit".to_owned());
    }
    read_limited(response)
}

fn read_limited(mut reader: impl Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .by_ref()
        .take(MAX_ENCODED_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read background image: {error}"))?;
    if bytes.len() > MAX_ENCODED_BYTES {
        Err("background image exceeds the encoded size limit".to_owned())
    } else if bytes.is_empty() {
        Err("background image is empty".to_owned())
    } else {
        Ok(bytes)
    }
}

fn decode(bytes: Vec<u8>, blur: f32) -> Result<egui::ColorImage, String> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| "background image format could not be detected".to_owned())?;
    if !matches!(
        reader.format(),
        Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
    ) {
        return Err("background image must be PNG, JPEG, or WebP".to_owned());
    }
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_DECODER_ALLOC);
    reader.limits(limits);
    let decoder = reader
        .into_decoder()
        .map_err(|error| format!("could not decode background image: {error}"))?;
    use image::ImageDecoder;
    let (width, height) = decoder.dimensions();
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("background image dimensions exceed the safety limit".to_owned());
    }
    let mut image = image::DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("could not decode background image: {error}"))?;
    if blur > 0.0 {
        image = image.blur(blur.clamp(0.0, MAX_BLUR_RADIUS));
    }
    let rgba = image.into_rgba8();
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        [rgba.width() as usize, rgba.height() as usize],
        rgba.as_raw(),
    ))
}

fn choose_folder_image(folder: &Path, selection_seed: u64) -> Result<PathBuf, String> {
    let entries = fs::read_dir(folder)
        .map_err(|error| format!("could not read background folder: {error}"))?;
    let mut candidates = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && supported_extension(path))
        .take(MAX_FOLDER_IMAGES + 1)
        .collect::<Vec<_>>();
    if candidates.len() > MAX_FOLDER_IMAGES {
        return Err(format!(
            "background folder contains more than {MAX_FOLDER_IMAGES} supported images"
        ));
    }
    candidates.sort();
    if candidates.is_empty() {
        return Err("background folder contains no PNG, JPEG, or WebP images".to_owned());
    }
    let mut hasher = DefaultHasher::new();
    selection_seed.hash(&mut hasher);
    folder.hash(&mut hasher);
    Ok(candidates[(hasher.finish() as usize) % candidates.len()].clone())
}

fn supported_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "png" | "jpg" | "jpeg" | "webp"
            )
        })
}

impl From<&BackgroundSettings> for PaintConfig {
    fn from(config: &BackgroundSettings) -> Self {
        let source = match &config.source {
            BackgroundSource::None => None,
            BackgroundSource::LocalFile { path } => Some(Source::File(path.clone())),
            BackgroundSource::HttpsUrl { url } => Some(Source::Url(url.clone())),
            BackgroundSource::RandomFolder { path } => Some(Source::Folder(path.clone())),
        };
        Self {
            enabled: source.is_some(),
            source,
            fit: match &config.fit {
                BackgroundFit::Cover => Fit::Cover,
                BackgroundFit::Contain => Fit::Contain,
                BackgroundFit::Stretch => Fit::Stretch,
                BackgroundFit::Tile => Fit::Tile,
            },
            horizontal: match &config.horizontal_alignment {
                HorizontalAlignment::Left => Horizontal::Left,
                HorizontalAlignment::Center => Horizontal::Center,
                HorizontalAlignment::Right => Horizontal::Right,
            },
            vertical: match &config.vertical_alignment {
                VerticalAlignment::Top => Vertical::Top,
                VerticalAlignment::Center => Vertical::Center,
                VerticalAlignment::Bottom => Vertical::Bottom,
            },
            opacity: finite_or(config.opacity, 1.0).clamp(0.0, 1.0),
            blur: finite_or(config.blur, 0.0).clamp(0.0, MAX_BLUR_RADIUS),
            overlay: parse_rgb(&config.overlay).unwrap_or(egui::Color32::BLACK),
            overlay_opacity: finite_or(config.overlay_opacity, 0.0).clamp(0.0, 1.0),
        }
    }
}

fn load_identity<K: Hash>(config: &BackgroundSettings, page_key: &K) -> u64 {
    let mut hasher = DefaultHasher::new();
    match &config.source {
        BackgroundSource::None => 0_u8.hash(&mut hasher),
        BackgroundSource::LocalFile { path } => {
            1_u8.hash(&mut hasher);
            path.hash(&mut hasher);
        }
        BackgroundSource::HttpsUrl { url } => {
            2_u8.hash(&mut hasher);
            url.hash(&mut hasher);
        }
        BackgroundSource::RandomFolder { path } => {
            3_u8.hash(&mut hasher);
            path.hash(&mut hasher);
        }
    }
    config.blur.to_bits().hash(&mut hasher);
    config.reload_nonce.hash(&mut hasher);
    page_key.hash(&mut hasher);
    hasher.finish()
}

fn finite_or(value: f32, default: f32) -> f32 {
    if value.is_finite() { value } else { default }
}

fn parse_rgb(value: &str) -> Option<egui::Color32> {
    let rgb = value.trim().strip_prefix('#')?;
    if rgb.len() != 6 {
        return None;
    }
    Some(egui::Color32::from_rgb(
        u8::from_str_radix(&rgb[0..2], 16).ok()?,
        u8::from_str_radix(&rgb[2..4], 16).ok()?,
        u8::from_str_radix(&rgb[4..6], 16).ok()?,
    ))
}

fn paint_texture(
    painter: &egui::Painter,
    rect: egui::Rect,
    texture: &egui::TextureHandle,
    config: &PaintConfig,
) {
    let image = texture.size_vec2();
    if image.x <= 0.0 || image.y <= 0.0 {
        return;
    }
    let tint = egui::Color32::from_white_alpha((config.opacity * 255.0).round() as u8);
    match config.fit {
        Fit::Stretch => {
            painter.image(texture.id(), rect, unit_uv(), tint);
        }
        Fit::Contain => {
            let scale = (rect.width() / image.x).min(rect.height() / image.y);
            let destination = aligned_rect(rect, image * scale, config.horizontal, config.vertical);
            painter.image(texture.id(), destination, unit_uv(), tint);
        }
        Fit::Cover => {
            let uv = cover_uv(rect.size(), image, config.horizontal, config.vertical);
            painter.image(texture.id(), rect, uv, tint);
        }
        Fit::Tile => paint_tiles(painter, rect, texture.id(), image, tint, config),
    }
}

fn aligned_rect(
    bounds: egui::Rect,
    size: egui::Vec2,
    horizontal: Horizontal,
    vertical: Vertical,
) -> egui::Rect {
    let x = match horizontal {
        Horizontal::Left => bounds.left(),
        Horizontal::Center => bounds.center().x - size.x * 0.5,
        Horizontal::Right => bounds.right() - size.x,
    };
    let y = match vertical {
        Vertical::Top => bounds.top(),
        Vertical::Center => bounds.center().y - size.y * 0.5,
        Vertical::Bottom => bounds.bottom() - size.y,
    };
    egui::Rect::from_min_size(egui::pos2(x, y), size)
}

fn cover_uv(
    destination: egui::Vec2,
    source: egui::Vec2,
    horizontal: Horizontal,
    vertical: Vertical,
) -> egui::Rect {
    let scale = (destination.x / source.x).max(destination.y / source.y);
    let visible = egui::vec2(
        (destination.x / (source.x * scale)).clamp(0.0, 1.0),
        (destination.y / (source.y * scale)).clamp(0.0, 1.0),
    );
    let x = alignment_offset(1.0 - visible.x, horizontal);
    let y = vertical_offset(1.0 - visible.y, vertical);
    egui::Rect::from_min_size(egui::pos2(x, y), visible)
}

fn alignment_offset(extra: f32, alignment: Horizontal) -> f32 {
    match alignment {
        Horizontal::Left => 0.0,
        Horizontal::Center => extra * 0.5,
        Horizontal::Right => extra,
    }
}

fn vertical_offset(extra: f32, alignment: Vertical) -> f32 {
    match alignment {
        Vertical::Top => 0.0,
        Vertical::Center => extra * 0.5,
        Vertical::Bottom => extra,
    }
}

fn paint_tiles(
    painter: &egui::Painter,
    bounds: egui::Rect,
    texture: egui::TextureId,
    tile: egui::Vec2,
    tint: egui::Color32,
    config: &PaintConfig,
) {
    // Tiny source images could otherwise create hundreds of thousands of paint commands.
    // Scale only those tiles up enough to cover the viewport within the primitive budget.
    let tile = bounded_tile_size(bounds.size(), tile);
    let anchor = aligned_rect(bounds, tile, config.horizontal, config.vertical).min;
    let start_x = tile_start(bounds.left(), anchor.x, tile.x);
    let start_y = tile_start(bounds.top(), anchor.y, tile.y);
    let clipped = painter.with_clip_rect(bounds);
    let mut y = start_y;
    while y < bounds.bottom() {
        let mut x = start_x;
        while x < bounds.right() {
            clipped.image(
                texture,
                egui::Rect::from_min_size(egui::pos2(x, y), tile),
                unit_uv(),
                tint,
            );
            x += tile.x;
        }
        y += tile.y;
    }
}

fn bounded_tile_size(bounds: egui::Vec2, mut tile: egui::Vec2) -> egui::Vec2 {
    let count = |tile: egui::Vec2| {
        ((bounds.x / tile.x).ceil() as usize + 1)
            .saturating_mul((bounds.y / tile.y).ceil() as usize + 1)
    };
    let required = count(tile);
    if required > MAX_TILES {
        tile *= (required as f32 / MAX_TILES as f32).sqrt();
        while count(tile) > MAX_TILES {
            tile *= 1.01;
        }
    }
    tile
}

fn tile_start(minimum: f32, anchor: f32, size: f32) -> f32 {
    anchor - ((anchor - minimum) / size).ceil().max(0.0) * size
}

fn normalized_alpha(color_alpha: u8, opacity: f32) -> u8 {
    (f32::from(color_alpha) * opacity.clamp(0.0, 1.0)).round() as u8
}

fn unit_uv() -> egui::Rect {
    egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_disable_all_background_painting() {
        let config = PaintConfig::from(&BackgroundSettings::default());
        assert!(!config.enabled);
        assert!(config.source.is_none());
        assert_eq!(config.overlay_opacity, 0.0);
    }

    #[test]
    fn converts_typed_background_settings() {
        let settings = BackgroundSettings {
            source: BackgroundSource::HttpsUrl {
                url: "https://example.com/a.webp".to_owned(),
            },
            fit: BackgroundFit::Contain,
            horizontal_alignment: HorizontalAlignment::Right,
            vertical_alignment: VerticalAlignment::Bottom,
            opacity: 0.75,
            blur: 4.0,
            overlay: "#123456".to_owned(),
            overlay_opacity: 0.25,
            reload_nonce: 0,
        };
        let config = PaintConfig::from(&settings);
        assert!(config.enabled);
        assert_eq!(
            config.source,
            Some(Source::Url("https://example.com/a.webp".to_owned()))
        );
        assert_eq!(config.fit, Fit::Contain);
        assert_eq!(config.horizontal, Horizontal::Right);
        assert_eq!(config.vertical, Vertical::Bottom);
        assert_eq!(config.overlay, egui::Color32::from_rgb(0x12, 0x34, 0x56));
    }

    #[test]
    fn load_identity_changes_with_page_source_blur_and_reload_only() {
        let mut settings = BackgroundSettings::default();
        let disabled = load_identity(&settings, &0_u8);
        settings.source = BackgroundSource::LocalFile {
            path: PathBuf::from("wallpaper.png"),
        };
        let source = load_identity(&settings, &0_u8);
        assert_ne!(disabled, source);
        assert_ne!(source, load_identity(&settings, &1_u8));
        settings.opacity = 0.4;
        settings.fit = BackgroundFit::Tile;
        assert_eq!(source, load_identity(&settings, &0_u8));
        settings.blur = 2.0;
        assert_ne!(source, load_identity(&settings, &0_u8));
        settings.blur = 0.0;
        settings.reload_nonce += 1;
        assert_ne!(source, load_identity(&settings, &0_u8));
    }

    #[test]
    fn cover_layout_crops_according_to_alignment() {
        let left = cover_uv(
            egui::vec2(100.0, 100.0),
            egui::vec2(200.0, 100.0),
            Horizontal::Left,
            Vertical::Center,
        );
        let right = cover_uv(
            egui::vec2(100.0, 100.0),
            egui::vec2(200.0, 100.0),
            Horizontal::Right,
            Vertical::Center,
        );
        assert_eq!(left.min.x, 0.0);
        assert_eq!(left.max.x, 0.5);
        assert_eq!(right.min.x, 0.5);
        assert_eq!(right.max.x, 1.0);
        assert_eq!(left.min.y, 0.0);
        assert_eq!(left.max.y, 1.0);
    }

    #[test]
    fn contain_layout_honors_alignment() {
        let bounds = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 100.0));
        let placed = aligned_rect(
            bounds,
            egui::vec2(50.0, 40.0),
            Horizontal::Right,
            Vertical::Bottom,
        );
        assert_eq!(placed.right(), bounds.right());
        assert_eq!(placed.bottom(), bounds.bottom());
    }

    #[test]
    fn source_extension_allowlist_is_case_insensitive() {
        assert!(supported_extension(Path::new("wallpaper.PNG")));
        assert!(supported_extension(Path::new("wallpaper.jpeg")));
        assert!(supported_extension(Path::new("wallpaper.WeBp")));
        assert!(!supported_extension(Path::new("wallpaper.gif")));
    }

    #[test]
    fn tile_origin_reaches_the_leading_edge() {
        assert_eq!(tile_start(10.0, 35.0, 20.0), -5.0);
        assert_eq!(tile_start(10.0, 10.0, 20.0), 10.0);
    }

    #[test]
    fn tiny_tiles_are_scaled_to_the_paint_budget() {
        let bounds = egui::vec2(1200.0, 800.0);
        let tile = bounded_tile_size(bounds, egui::vec2(1.0, 1.0));
        let count =
            ((bounds.x / tile.x).ceil() as usize + 1) * ((bounds.y / tile.y).ceil() as usize + 1);
        assert!(count <= MAX_TILES);
        assert!(tile.x > 1.0);
    }
}
