//! # Fabric loader support.
//!
//! Fabric doesn't ship its own game — it layers a small loader jar plus
//! a handful of libraries on top of an existing vanilla install and
//! swaps the main class. Rather than teaching `crate::minecraft` about
//! that, this module builds a completely ordinary vanilla-shaped
//! version out of it:
//!
//! 1. Make sure the vanilla version is installed (`minecraft::install_version`).
//! 2. Ask Fabric's meta API for the latest stable loader build for that
//!    Minecraft version, and fetch its "profile" JSON (main class +
//!    Fabric's own libraries, in Maven-coordinate form).
//! 3. Read the vanilla version's *own* metadata JSON back off disk (the
//!    file `install_version` already wrote) and merge it with Fabric's
//!    profile into a synthetic version id, e.g.
//!    `fabric-loader-0.15.11-1.21.1`: same assets/downloads/JVM-args as
//!    vanilla, `mainClass` and extra libraries from Fabric.
//! 4. Write that merged metadata to its own version directory, copy the
//!    vanilla `client.jar` into it, and download Fabric's extra
//!    libraries into the *shared* `libraries/` folder vanilla uses.
//!
//! From there, `crate::minecraft::launch_version` and
//! `is_version_installed` work on the synthetic id completely
//! unmodified — this module never touches the game process directly.
//!
//! After copying `client.jar`, we also `copy_natives` from the vanilla
//! version into the synthetic id. `launch_version` looks for natives
//! under `natives/<this version's id>`, but extraction only happens
//! for the vanilla id during `install_version`.
//!
//! The synthetic version and Fabric dependencies use the same relative
//! `minecraft/versions/` and `minecraft/libraries/` storage as vanilla. A
//! `fabric-loader.txt` marker is written only after metadata, client, natives,
//! and libraries complete successfully. Earlier failures may leave reusable
//! partial files, but dispatch will treat the loader as uninstalled until that
//! marker commits the composite id.

use crate::core::paths::AppPaths;
use crate::minecraft::{self, FerriteError, Result};
use reqwest::blocking::Client;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

const META_BASE: &str = "https://meta.fabricmc.net/v2/versions/loader";

/// Installs the latest stable Fabric loader for `mc_version`, on top of
/// the vanilla install (installing that first if it isn't already
/// present).
pub fn install(paths: &AppPaths, mc_version: &str) -> Result<()> {
    install_version(paths, mc_version, None)
}

/// Installs a requested Fabric version, or discovers a stable build when omitted.
///
/// A non-empty requested version is used verbatim and validated by fetching its
/// profile. With no request, Fabric's API order is trusted: the first stable
/// entry wins, falling back to the first published entry if none is marked
/// stable. Network, metadata, and filesystem errors abort without writing the
/// marker, although the preceding vanilla install and partial cache remain.
pub fn install_version(paths: &AppPaths, mc_version: &str, requested: Option<&str>) -> Result<()> {
    minecraft::install_version(paths, mc_version)?;

    let client = Client::new();
    let loader_version = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        Some(version) => version.to_owned(),
        None => latest_stable_loader_version(&client, mc_version)?,
    };
    let composite_id = composite_id(mc_version, &loader_version);

    println!("Fetching Fabric profile for loader {loader_version}...");
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

    println!("Downloading Fabric loader libraries...");
    download_fabric_libraries(paths, &client, &profile)?;

    fs::write(marker_path(paths, mc_version), &composite_id)?;
    println!("Fabric {loader_version} installed for Minecraft {mc_version}.");
    Ok(())
}

/// The synthetic version id of the merged vanilla+Fabric install
/// currently recorded for `mc_version`. Errors with
/// `FerriteError::LoaderNotInstalled` if Fabric hasn't been installed
/// for it yet.
pub fn installed_composite_id(paths: &AppPaths, mc_version: &str) -> Result<String> {
    fs::read_to_string(marker_path(paths, mc_version))
        .map_err(|_| FerriteError::LoaderNotInstalled(mc_version.to_string()))
}

/// Where we record which composite id is currently installed for a
/// given vanilla version, so `launch`/`is_installed` don't need to
/// re-query Fabric's meta API (and stay correct even if a newer loader
/// build gets published between install and launch).
fn marker_path(paths: &AppPaths, mc_version: &str) -> PathBuf {
    paths.version_dir(mc_version).join("fabric-loader.txt")
}

fn composite_id(mc_version: &str, loader_version: &str) -> String {
    format!("fabric-loader-{loader_version}-{mc_version}")
}

// ---------------------------------------------------------------------
// Fabric meta API
// ---------------------------------------------------------------------

#[derive(Deserialize)]
struct LoaderListEntry {
    loader: LoaderInfo,
}

#[derive(Deserialize)]
struct LoaderInfo {
    version: String,
    stable: bool,
}

/// Selects the first stable API entry, or the first entry of any kind. An empty
/// response is reported as `LoaderVersionUnavailable`; HTTP and JSON failures
/// retain their more specific shared error variants.
fn latest_stable_loader_version(client: &Client, mc_version: &str) -> Result<String> {
    let url = format!("{META_BASE}/{mc_version}");
    let text = client.get(&url).send()?.error_for_status()?.text()?;

    let entries: Vec<LoaderListEntry> = serde_json::from_str(&text)?;
    entries
        .iter()
        .find(|e| e.loader.stable)
        .or_else(|| entries.first())
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

/// Builds a synthetic vanilla-shaped version metadata JSON.
///
/// The function clones vanilla so assets, downloads, Java requirements, and JVM
/// arguments remain owned by the result. It then replaces `id` and, when
/// present, `mainClass`; appends convertible Fabric libraries and Fabric game
/// arguments in profile order. Malformed optional profile sections are skipped,
/// leaving the corresponding vanilla data intact.
fn merge_metadata(
    composite_id: &str,
    vanilla: &serde_json::Value,
    fabric_profile: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = vanilla.clone();

    merged["id"] = serde_json::Value::String(composite_id.to_string());

    if let Some(main_class) = fabric_profile.get("mainClass") {
        merged["mainClass"] = main_class.clone();
    }

    // Libraries: vanilla's (already in `downloads.artifact` shape) plus
    // Fabric's (converted from Maven-coordinate form).
    let mut libraries = vanilla["libraries"].as_array().cloned().unwrap_or_default();
    if let Some(fabric_libs) = fabric_profile["libraries"].as_array() {
        for lib in fabric_libs {
            if let Some(converted) = super::metadata::convert_profile_library(lib) {
                libraries.push(converted);
            }
        }
    }
    merged["libraries"] = serde_json::Value::Array(libraries);

    // Arguments: keep vanilla's JVM args untouched (natives-directory
    // setup etc. is identical); append Fabric's game args (usually
    // empty for modern loader versions) after vanilla's.
    let mut game_args = vanilla["arguments"]["game"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(fabric_game) = fabric_profile["arguments"]["game"].as_array() {
        game_args.extend(fabric_game.iter().cloned());
    }
    merged["arguments"]["game"] = serde_json::Value::Array(game_args);

    merged
}

// ---------------------------------------------------------------------
// Library download
// ---------------------------------------------------------------------

/// Downloads profile libraries into the shared Maven cache.
///
/// A missing library array is a valid no-op. Entries without a usable name, URL,
/// or coordinate are skipped. Existing destination paths are trusted without a
/// size/hash check; the first HTTP or filesystem failure aborts the loop.
fn download_fabric_libraries(
    paths: &AppPaths,
    client: &Client,
    profile: &serde_json::Value,
) -> Result<()> {
    super::metadata::download_profile_libraries(paths, client, profile, "fabric")
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
            download_fabric_libraries(&paths, &Client::new(), &profile),
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
            "libraries": [{"name": "org.example:loader:2:universal", "url": "https://maven.fabricmc.net/"}],
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
        assert_eq!(merged["arguments"]["jvm"], json!(["-Dvanilla=true"]));
        assert_eq!(merged["libraries"].as_array().unwrap().len(), 2);
        assert_eq!(merged["libraries"][0], vanilla["libraries"][0]);
        assert_eq!(
            merged["libraries"][1]["downloads"]["artifact"],
            json!({
                "path": "org/example/loader/2/loader-2-universal.jar",
                "url": "https://maven.fabricmc.net/org/example/loader/2/loader-2-universal.jar",
                "size": 0
            })
        );
    }
}
