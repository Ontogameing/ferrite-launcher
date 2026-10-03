//! # NeoForge loader support.
//!
//! NeoForge used two different Maven layouts:
//!
//! - **1.20.1 only** (`net.neoforged:forge`): versions look like
//!   `{MC}-{NEO}` (`1.20.1-47.1.106`), same as old Forge.
//! - **1.20.2+** (`net.neoforged:neoforge`): versions look like
//!   `{minor}.{patch}.{build}` with no `1.` prefix, e.g. Minecraft
//!   `1.21.11` → NeoForge `21.11.x`.
//!
//! In both cases what Maven actually ships is an installer JAR, not a
//! standalone version JSON. We run
//!
//! ```text
//! java -jar neoforge-…-installer.jar --installClient <minecraft/>
//! ```
//!
//! then merge the resulting `inheritsFrom` JSON with vanilla the same
//! way `forge.rs` does, so `crate::minecraft::launch_version` can run
//! the game without knowing NeoForge exists.
//!
//! All output uses Ferrite's relative shared `minecraft/` tree. The exact
//! installer-created id is persisted in `neoforge-loader.txt` beside the vanilla
//! version only after normalization, client selection, and native copying. Thus
//! failed installs can leave reusable artifacts but are not launchable through
//! dispatch until the marker is committed.

use crate::minecraft::{self, FerriteError, Result};
use ferrite_launcher::core::paths::AppPaths;
use reqwest::blocking::Client;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const NEO_MAVEN: &str = "https://maven.neoforged.net/releases";
const LEGACY_META: &str =
    "https://maven.neoforged.net/releases/net/neoforged/forge/maven-metadata.xml";
const MODERN_META: &str =
    "https://maven.neoforged.net/releases/net/neoforged/neoforge/maven-metadata.xml";

/// Maven artifact family and version representation selected for an install.
/// Owning the strings keeps the value independent of temporary metadata buffers.
enum NeoCoord {
    /// `net.neoforged:forge:{mc}-{neo}` (Minecraft 1.20.1).
    Legacy { full: String },
    /// `net.neoforged:neoforge:{neo}` (Minecraft 1.20.2+).
    Modern { neo: String },
}

/// Installs the latest available NeoForge build for `mc_version`.
pub fn install(paths: &AppPaths, mc_version: &str) -> Result<()> {
    install_version(paths, mc_version, None)
}

/// Installs a requested NeoForge version, or discovers the latest compatible build.
///
/// Minecraft 1.20.1 pins use the legacy `forge` artifact and are normalized to a
/// full `1.20.1-<neo>` coordinate; newer pins use `neoforge` verbatim. The
/// pipeline installs vanilla, satisfies official-installer prerequisites, runs
/// Java synchronously, discovers its output, flattens inherited metadata, ensures
/// a client jar, copies natives, and writes the marker last. Errors leave any
/// completed shared-cache work in place. Installer-file removal is best-effort.
pub fn install_version(paths: &AppPaths, mc_version: &str, requested: Option<&str>) -> Result<()> {
    minecraft::install_version(paths, mc_version)?;

    let client = Client::new();
    let coord = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(version) if mc_version == "1.20.1" => NeoCoord::Legacy {
            full: if version.starts_with("1.20.1-") {
                version.to_owned()
            } else {
                format!("1.20.1-{version}")
            },
        },
        Some(version) => NeoCoord::Modern {
            neo: version.to_owned(),
        },
        None => latest_neoforge_coord(&client, mc_version)?,
    };
    let neo_label = match &coord {
        NeoCoord::Legacy { full } => full.clone(),
        NeoCoord::Modern { neo } => neo.clone(),
    };
    println!("Latest NeoForge for {mc_version} is {neo_label}");

    let vanilla_dir = paths.version_dir(mc_version);
    let vanilla_client = vanilla_dir.join("client.jar");
    let vanilla_named = vanilla_dir.join(format!("{mc_version}.jar"));
    if !vanilla_named.exists() {
        fs::copy(&vanilla_client, &vanilla_named)?;
    }

    let installer_url = match &coord {
        NeoCoord::Legacy { full } => {
            format!("{NEO_MAVEN}/net/neoforged/forge/{full}/forge-{full}-installer.jar")
        }
        NeoCoord::Modern { neo } => {
            format!("{NEO_MAVEN}/net/neoforged/neoforge/{neo}/neoforge-{neo}-installer.jar")
        }
    };
    let installer_name = match &coord {
        NeoCoord::Legacy { full } => format!("forge-{full}-installer.jar"),
        NeoCoord::Modern { neo } => format!("neoforge-{neo}-installer.jar"),
    };
    let installer_path = paths.downloads_dir().join(installer_name);
    if let Some(parent) = installer_path.parent() {
        fs::create_dir_all(parent)?;
    }
    println!("Downloading NeoForge installer...");
    minecraft::download_file(&client, &installer_url, &installer_path, None)?;

    // Same check as Forge: the installer refuses to run without this file.
    ensure_launcher_profiles(paths.storage_root())?;

    println!("Running NeoForge installer (this can take a while)...");
    run_installer(&installer_path, paths.storage_root())?;
    let _ = fs::remove_file(&installer_path);

    let composite_id = find_installed_neoforge_id(paths, mc_version, &neo_label)?;
    println!("NeoForge installer created version id `{composite_id}`");

    let composite_dir = paths.version_dir(&composite_id);
    let json_path = composite_dir.join(format!("{composite_id}.json"));
    let neo_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&json_path)?)?;

    let vanilla_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        vanilla_dir.join(format!("{mc_version}.json")),
    )?)?;

    let merged = merge_inherited_metadata(&composite_id, &vanilla_json, &neo_json);
    fs::write(&json_path, serde_json::to_string_pretty(&merged)?)?;

    ensure_client_jar(&composite_dir, &composite_id, &vanilla_client)?;
    minecraft::copy_natives(paths, mc_version, &composite_id)?;

    fs::write(marker_path(paths, mc_version), &composite_id)?;
    println!("NeoForge {neo_label} installed for Minecraft {mc_version}.");
    Ok(())
}

/// Reads the installer-created synthetic id recorded for `mc_version`.
/// Any marker read failure is intentionally exposed as `LoaderNotInstalled`.
pub fn installed_composite_id(paths: &AppPaths, mc_version: &str) -> Result<String> {
    fs::read_to_string(marker_path(paths, mc_version))
        .map_err(|_| FerriteError::LoaderNotInstalled(mc_version.to_string()))
}

fn marker_path(paths: &AppPaths, mc_version: &str) -> PathBuf {
    paths.version_dir(mc_version).join("neoforge-loader.txt")
}

// ---------------------------------------------------------------------
// Version discovery
// ---------------------------------------------------------------------

/// Selects a coordinate from Maven metadata.
///
/// The 1.20.1 path takes the last exact legacy match. Modern Minecraft ids are
/// converted to NeoForge's version prefix and the last exact/dot/dash-prefixed
/// match is selected. Repository order is assumed oldest-to-newest. Unsupported
/// id shapes and empty matches return `LoaderVersionUnavailable`.
fn latest_neoforge_coord(client: &Client, mc_version: &str) -> Result<NeoCoord> {
    // 1.20.1 still lives under the old `net.neoforged:forge` artifact.
    if mc_version == "1.20.1" {
        let text = client.get(LEGACY_META).send()?.error_for_status()?.text()?;
        let full = xml_versions(&text)
            .into_iter()
            .filter(|v| v.split('-').next() == Some("1.20.1"))
            .last()
            .ok_or_else(|| FerriteError::LoaderVersionUnavailable(mc_version.to_string()))?;
        return Ok(NeoCoord::Legacy { full });
    }

    // Modern: Minecraft 1.21.11 → NeoForge versions 21.11.*
    let Some(prefix) = modern_prefix(mc_version) else {
        return Err(FerriteError::LoaderVersionUnavailable(
            mc_version.to_string(),
        ));
    };
    let text = client.get(MODERN_META).send()?.error_for_status()?.text()?;
    let neo = xml_versions(&text)
        .into_iter()
        .filter(|v| {
            v == &prefix
                || v.starts_with(&format!("{prefix}."))
                || v.starts_with(&format!("{prefix}-"))
        })
        .last()
        .ok_or_else(|| FerriteError::LoaderVersionUnavailable(mc_version.to_string()))?;
    Ok(NeoCoord::Modern { neo })
}

/// `"1.21.11"` → `"21.11"`, `"1.21"` → `"21.0"` (NeoForge pads a missing
/// patch with `.0`).
fn modern_prefix(mc_version: &str) -> Option<String> {
    let rest = mc_version.strip_prefix('1')?.strip_prefix('.')?;
    if rest.is_empty() {
        return None;
    }
    if rest.contains('.') {
        Some(rest.to_string())
    } else {
        Some(format!("{rest}.0"))
    }
}

fn xml_versions(xml: &str) -> Vec<String> {
    xml.split("<version>")
        .filter_map(|chunk| {
            let end = chunk.find("</version>")?;
            Some(chunk[..end].to_string())
        })
        .collect()
}

// ---------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------

fn ensure_launcher_profiles(minecraft_dir: &Path) -> Result<()> {
    let path = minecraft_dir.join("launcher_profiles.json");
    if path.exists() {
        return Ok(());
    }
    fs::create_dir_all(minecraft_dir)?;
    fs::write(
        path,
        r#"{
  "profiles": {},
  "selectedProfile": "",
  "clientToken": "",
  "launcherVersion": {
    "name": "ferrite-launcher",
    "format": 21,
    "profilesFormat": 2
  }
}"#,
    )?;
    Ok(())
}

/// Runs the official installer synchronously using canonical paths. Spawn and
/// filesystem failures propagate as `Io`; non-zero Java exits become
/// `InstallerFailed` containing trimmed stdout and stderr. Successful output is
/// intentionally not parsed here because the installer chooses the version id.
fn run_installer(installer: &Path, minecraft_dir: &Path) -> Result<()> {
    fs::create_dir_all(minecraft_dir)?;
    let minecraft_abs = fs::canonicalize(minecraft_dir)?;
    let installer_abs = fs::canonicalize(installer)?;

    let output = Command::new("java")
        .arg("-jar")
        .arg(&installer_abs)
        .arg("--installClient")
        .arg(&minecraft_abs)
        .output()?;

    if !output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(FerriteError::InstallerFailed(
            format!("{stdout}{stderr}").trim().to_string(),
        ));
    }
    Ok(())
}

/// Discovers the version directory produced by the installer.
///
/// Common legacy and modern names are tried first. The fallback scans Forge- or
/// NeoForge-named directories for JSON mentioning the selected build and enough
/// evidence of the requested parent. Unreadable entries are skipped. No match
/// after a successful installer run is classified as `InstallerFailed`.
fn find_installed_neoforge_id(
    paths: &AppPaths,
    mc_version: &str,
    neo_label: &str,
) -> Result<String> {
    let candidates = [
        format!("neoforge-{neo_label}"),
        format!("{mc_version}-neoforge-{neo_label}"),
        format!("{mc_version}-forge-{neo_label}"),
        format!("{mc_version}-NeoForge{neo_label}"),
        neo_label.to_string(),
    ];
    for id in &candidates {
        if paths.version_dir(id).join(format!("{id}.json")).exists() {
            return Ok(id.clone());
        }
    }

    let versions_root = paths.versions_dir();
    if let Ok(entries) = fs::read_dir(&versions_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let lower = name.to_ascii_lowercase();
            if !lower.contains("neoforge") && !lower.contains("forge") {
                continue;
            }
            let json_path = entry.path().join(format!("{name}.json"));
            let Ok(text) = fs::read_to_string(json_path) else {
                continue;
            };
            if text.contains(neo_label)
                && (text.contains(&format!("\"inheritsFrom\": \"{mc_version}\""))
                    || name.contains(mc_version)
                    || lower.contains("neoforge"))
            {
                return Ok(name);
            }
        }
    }

    Err(FerriteError::InstallerFailed(format!(
        "NeoForge installer finished but no version folder was found for {mc_version} / {neo_label}"
    )))
}

/// Ensures `client.jar` exists, preferring the existing normalized client, then
/// the installer's id-named jar, and finally the vanilla client.
fn ensure_client_jar(
    composite_dir: &Path,
    composite_id: &str,
    vanilla_client: &Path,
) -> Result<()> {
    let client_jar = composite_dir.join("client.jar");
    if client_jar.exists() {
        return Ok(());
    }
    let version_jar = composite_dir.join(format!("{composite_id}.jar"));
    if version_jar.exists() {
        fs::copy(version_jar, client_jar)?;
        return Ok(());
    }
    fs::copy(vanilla_client, client_jar)?;
    Ok(())
}

// ---------------------------------------------------------------------
// Metadata merging
// ---------------------------------------------------------------------

/// Clones vanilla metadata and flattens NeoForge's inherited overlay.
///
/// The result owns all JSON data, removes `inheritsFrom`, adopts NeoForge's main
/// class when present, appends normalized libraries, and appends game and JVM
/// arguments in source order. Missing optional overlay sections preserve the
/// vanilla values.
fn merge_inherited_metadata(
    composite_id: &str,
    vanilla: &serde_json::Value,
    neo: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = vanilla.clone();
    merged["id"] = serde_json::Value::String(composite_id.to_string());
    merged.as_object_mut().map(|o| o.remove("inheritsFrom"));

    if let Some(main_class) = neo.get("mainClass") {
        merged["mainClass"] = main_class.clone();
    }

    let mut libraries = vanilla["libraries"].as_array().cloned().unwrap_or_default();
    if let Some(neo_libs) = neo["libraries"].as_array() {
        for lib in neo_libs {
            libraries.push(normalize_library(lib));
        }
    }
    merged["libraries"] = serde_json::Value::Array(libraries);

    append_args(&mut merged, vanilla, neo, "game");
    append_args(&mut merged, vanilla, neo, "jvm");

    merged
}

/// Appends one argument category and avoids manufacturing an empty array.
fn append_args(
    merged: &mut serde_json::Value,
    vanilla: &serde_json::Value,
    neo: &serde_json::Value,
    kind: &str,
) {
    let mut args = vanilla["arguments"][kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(extra) = neo
        .get("arguments")
        .and_then(|a| a.get(kind))
        .and_then(|g| g.as_array())
    {
        args.extend(extra.iter().cloned());
    }
    if !args.is_empty() {
        merged["arguments"][kind] = serde_json::Value::Array(args);
    }
}

/// Converts coordinate-only installer libraries into the artifact shape consumed
/// by `minecraft.rs`. Already-normalized entries and unparseable coordinates are
/// cloned unchanged; missing repositories fall back to NeoForge Maven.
fn normalize_library(lib: &serde_json::Value) -> serde_json::Value {
    if lib
        .get("downloads")
        .and_then(|d| d.get("artifact"))
        .and_then(|a| a.get("path"))
        .is_some()
    {
        return lib.clone();
    }

    let Some(name) = lib.get("name").and_then(|v| v.as_str()) else {
        return lib.clone();
    };
    let Some(path) = maven_coordinate_to_path(name) else {
        return lib.clone();
    };
    let repo = lib
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("https://maven.neoforged.net/releases/");
    let url = format!("{}/{path}", repo.trim_end_matches('/'));

    let mut out = lib.clone();
    out["downloads"] = serde_json::json!({
        "artifact": {
            "path": path,
            "url": url,
            "size": 0
        }
    });
    if out.get("rules").is_none() {
        out["rules"] = serde_json::json!([]);
    }
    out
}

/// Converts `group:artifact:version[:classifier]` to a Maven cache path.
/// Missing required fields return `None`; later fields are ignored.
fn maven_coordinate_to_path(coordinate: &str) -> Option<String> {
    let mut parts = coordinate.split(':');
    let group = parts.next()?;
    let artifact = parts.next()?;
    let version = parts.next()?;
    let classifier = parts.next();
    let group_path = group.replace('.', "/");
    let file_name = match classifier {
        Some(c) => format!("{artifact}-{version}-{c}.jar"),
        None => format!("{artifact}-{version}.jar"),
    };
    Some(format!("{group_path}/{artifact}/{version}/{file_name}"))
}
