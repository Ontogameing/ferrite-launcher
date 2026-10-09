//! Quilt loader support.
//!
//! Quilt is a fork of Fabric and keeps essentially the same shape: a
//! meta API that hands back a ready-to-use "profile" JSON (main class +
//! libraries in Maven-coordinate form) for a given Minecraft version +
//! loader build. This module is just `fabric.rs`'s approach pointed at
//! Quilt's endpoints — see that module's docs for the full rationale
//! (merging into a synthetic vanilla-shaped version rather than
//! teaching `crate::minecraft` about loaders at all).
//!
//! After copying `client.jar`, we also `copy_natives` from the vanilla
//! version into the synthetic id — same reason as `fabric.rs`.
//!
//! Loader libraries are cached in the shared `minecraft/libraries/` Maven tree.
//! `minecraft/versions/<mc>/quilt-loader.txt` is the local source of truth for
//! the installed synthetic id and is written only after the complete pipeline.
//! A failed attempt can leave reusable metadata or jars, but no marker means
//! dispatch reports Quilt as uninstalled.

use crate::core::paths::AppPaths;
use crate::minecraft::{self, FerriteError, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

const META_BASE: &str = "https://meta.quiltmc.org/v3/versions/loader";

/// Installs the latest stable Quilt loader for `mc_version`, on top of
/// the vanilla install (installing that first if it isn't already
/// present).
pub fn install(paths: &AppPaths, mc_version: &str) -> Result<()> {
    install_version(paths, mc_version, None)
}

/// Installs a requested Quilt version, or discovers the newest stable build.
///
/// Empty requested strings behave like no pin. A supplied version is validated
/// when its profile is fetched. Without one, API order is treated as oldest to
/// newest: the last stable entry wins, falling back to the final entry. Any
/// error aborts before the marker is written, while completed vanilla/cache work
/// remains available for a retry.
pub fn install_version(paths: &AppPaths, mc_version: &str, requested: Option<&str>) -> Result<()> {
    minecraft::install_version(paths, mc_version)?;

    let client = Client::new();
    let loader_version = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(version) => version.to_owned(),
        None => latest_stable_loader_version(&client, mc_version)?,
    };
    let composite_id = composite_id(mc_version, &loader_version);

    println!("Fetching Quilt profile for loader {loader_version}...");
    let profile = fetch_profile(&client, mc_version, &loader_version)?;

    let vanilla_dir = paths.version_dir(mc_version);
    let vanilla_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        vanilla_dir.join(format!("{mc_version}.json")),
    )?)?;

    let merged = merge_metadata(&composite_id, &vanilla_json, &profile);

    let composite_dir = paths.version_dir(&composite_id);
    fs::create_dir_all(&composite_dir)?;
    fs::write(
        composite_dir.join(format!("{composite_id}.json")),
        serde_json::to_string(&merged)?,
    )?;
    fs::copy(
        vanilla_dir.join("client.jar"),
        composite_dir.join("client.jar"),
    )?;
    minecraft::copy_natives(paths, mc_version, &composite_id)?;

    println!("Downloading Quilt loader libraries...");
    download_quilt_libraries(paths, &client, &profile)?;

    fs::write(marker_path(paths, mc_version), &composite_id)?;
    println!("Quilt {loader_version} installed for Minecraft {mc_version}.");
    Ok(())
}

/// The synthetic version id of the merged vanilla+Quilt install
/// currently recorded for `mc_version`. Errors with
/// `FerriteError::LoaderNotInstalled` if Quilt hasn't been installed
/// for it yet.
pub fn installed_composite_id(paths: &AppPaths, mc_version: &str) -> Result<String> {
    fs::read_to_string(marker_path(paths, mc_version))
        .map_err(|_| FerriteError::LoaderNotInstalled(mc_version.to_string()))
}

fn marker_path(paths: &AppPaths, mc_version: &str) -> PathBuf {
    paths.version_dir(mc_version).join("quilt-loader.txt")
}

fn composite_id(mc_version: &str, loader_version: &str) -> String {
    format!("quilt-loader-{loader_version}-{mc_version}")
}

// ---------------------------------------------------------------------
// Quilt meta API
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct LoaderListEntry {
    loader: LoaderInfo,
}

#[derive(Deserialize)]
struct LoaderInfo {
    version: String,
    /// Present on Quilt's meta responses same as Fabric's, but kept
    /// `#[serde(default)]` in case a given build omits it.
    #[serde(default)]
    stable: bool,
}

/// Selects the newest stable entry from Quilt's oldest-first response, or the
/// newest entry when no build is marked stable. Empty responses become
/// `LoaderVersionUnavailable`; request and parse errors propagate unchanged.
fn latest_stable_loader_version(client: &Client, mc_version: &str) -> Result<String> {
    let url = format!("{META_BASE}/{mc_version}");
    let text = client.get(&url).send()?.error_for_status()?.text()?;

    let entries: Vec<LoaderListEntry> = serde_json::from_str(&text)?;
    // Quilt's meta API lists builds oldest-first. Prefer the last
    // `stable` entry; if none are marked stable, take the last entry
    // overall (the newest published build). Using `.first()` here is
    // what previously installed 0.20.0-beta.9 for 1.21.11, which
    // crashes on modern Java with LaunchClassLoader.
    entries
        .iter()
        .rev()
        .find(|e| e.loader.stable)
        .or_else(|| entries.last())
        .map(|e| e.loader.version.clone())
        .ok_or_else(|| FerriteError::LoaderVersionUnavailable(mc_version.to_string()))
}

fn fetch_profile(
    client: &Client,
    mc_version: &str,
    loader_version: &str,
) -> Result<serde_json::Value> {
    let url = format!("{META_BASE}/{mc_version}/{loader_version}/profile/json");
    let text = client.get(&url).send()?.error_for_status()?.text()?;
    Ok(serde_json::from_str(&text)?)
}

// ---------------------------------------------------------------------
// Metadata merging
// ---------------------------------------------------------------------

/// Builds an owned, vanilla-shaped metadata document for Quilt.
///
/// Vanilla is cloned first, preserving assets, downloads, and Java requirements.
/// The synthetic id and optional Quilt main class replace their vanilla fields;
/// convertible libraries and both game and JVM arguments are appended in profile
/// order. Keeping Quilt JVM arguments is required for modern Knot/Mixin startup.
/// Missing or malformed optional arrays simply contribute no additional values.
fn merge_metadata(
    composite_id: &str,
    vanilla: &serde_json::Value,
    quilt_profile: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = vanilla.clone();

    merged["id"] = serde_json::Value::String(composite_id.to_string());

    if let Some(main_class) = quilt_profile.get("mainClass") {
        merged["mainClass"] = main_class.clone();
    }

    let mut libraries = vanilla["libraries"].as_array().cloned().unwrap_or_default();
    if let Some(quilt_libs) = quilt_profile["libraries"].as_array() {
        for lib in quilt_libs {
            if let Some(converted) = super::metadata::convert_profile_library(lib) {
                libraries.push(converted);
            }
        }
    }
    merged["libraries"] = serde_json::Value::Array(libraries);

    let mut game_args = vanilla["arguments"]["game"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(quilt_game) = quilt_profile["arguments"]["game"].as_array() {
        game_args.extend(quilt_game.iter().cloned());
    }
    merged["arguments"]["game"] = serde_json::Value::Array(game_args);

    // Knot also ships extra JVM args (e.g. --add-opens). Dropping them
    // is a common cause of Mixin/ServiceLoader crashes on modern Java.
    let mut jvm_args = vanilla["arguments"]["jvm"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(quilt_jvm) = quilt_profile["arguments"]["jvm"].as_array() {
        jvm_args.extend(quilt_jvm.iter().cloned());
    }
    if !jvm_args.is_empty() {
        merged["arguments"]["jvm"] = serde_json::Value::Array(jvm_args);
    }

    merged
}

// ---------------------------------------------------------------------
// Library download
// ---------------------------------------------------------------------

/// Populates the shared library cache from Quilt's profile.
///
/// No library array is a successful no-op. Malformed entries are skipped,
/// existing paths are trusted, and the first download/filesystem error aborts.
fn download_quilt_libraries(
    paths: &AppPaths,
    client: &Client,
    profile: &serde_json::Value,
) -> Result<()> {
    super::metadata::download_profile_libraries(paths, client, profile, "quilt")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn library_download_rejects_cached_absolute_maven_group() {
        let temp = tempfile::Builder::new()
            .prefix("ferrite-maven-")
            .tempdir()
            .unwrap();
        let paths = AppPaths::from_base_dirs(crate::core::paths::BaseDirs {
            config: temp.path().join("config"),
            data_local: temp.path().join("data"),
            cache: temp.path().join("cache"),
        })
        .unwrap();
        let outside = temp.path().join("loader/1/loader-1.jar");
        fs::create_dir_all(outside.parent().unwrap()).unwrap();
        fs::write(&outside, b"cached outside library").unwrap();
        // Keep the rooted path without a Windows drive prefix, whose colon would
        // otherwise be interpreted as a Maven coordinate separator.
        let group: PathBuf = temp
            .path()
            .components()
            .filter(|component| !matches!(component, std::path::Component::Prefix(_)))
            .collect();
        let profile = json!({"libraries": [{
            "name": format!("{}:loader:1", group.display()),
            "url": "https://example.invalid/"
        }]});
        assert!(matches!(
            download_quilt_libraries(&paths, &Client::new(), &profile),
            Err(minecraft::FerriteError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData
        ));
        assert_eq!(fs::read(outside).unwrap(), b"cached outside library");
    }

    #[test]
    fn merged_metadata_preserves_vanilla_launch_data_and_loader_order() {
        let vanilla = json!({
            "id": "1.21.1", "mainClass": "net.minecraft.client.main.Main",
            "assets": "17", "assetIndex": {"id": "17", "url": "https://example.invalid/assets"},
            "downloads": {"client": {"url": "https://example.invalid/client.jar", "size": 42}},
            "libraries": [{"name": "org.example:vanilla:1", "downloads": {
                "artifact": {"path": "vanilla.jar", "url": "https://example.invalid/vanilla.jar", "size": 12}
            }}],
            "arguments": {"game": ["--username", "${auth_player_name}"], "jvm": ["-Dvanilla=true"]}
        });
        let loader = json!({
            "mainClass": "org.example.Loader",
            "libraries": [{"name": "org.example:loader:2:universal", "url": "https://maven.quiltmc.org/repository/release/"}],
            "arguments": {"game": ["--loader", "enabled"], "jvm": ["--add-opens=java.base/java.lang=ALL-UNNAMED"]}
        });

        let merged = merge_metadata("composite", &vanilla, &loader);

        assert_eq!(merged["id"], "composite");
        assert_eq!(merged["mainClass"], "org.example.Loader");
        for field in ["assets", "assetIndex", "downloads"] {
            assert_eq!(merged[field], vanilla[field], "{field}");
        }
        assert_eq!(
            merged["arguments"]["game"],
            json!(["--username", "${auth_player_name}", "--loader", "enabled"])
        );
        assert_eq!(
            merged["arguments"]["jvm"],
            json!([
                "-Dvanilla=true",
                "--add-opens=java.base/java.lang=ALL-UNNAMED"
            ])
        );
        assert_eq!(merged["libraries"].as_array().unwrap().len(), 2);
        assert_eq!(merged["libraries"][0], vanilla["libraries"][0]);
        assert_eq!(
            merged["libraries"][1]["downloads"]["artifact"],
            json!({
                "path": "org/example/loader/2/loader-2-universal.jar",
                "url": "https://maven.quiltmc.org/repository/release/org/example/loader/2/loader-2-universal.jar",
                "size": 0
            })
        );
    }
}
