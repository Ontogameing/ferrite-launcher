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

use crate::minecraft::{self, FerriteError, Result};
use ferrite_launcher::core::paths::AppPaths;
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
            if let Some(converted) = convert_fabric_library(lib) {
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

/// Converts a Fabric-meta library entry
/// (`{"name": "group:artifact:version", "url": "https://repo/"}`) into
/// the `downloads.artifact.{path,url,size}` shape
/// `crate::minecraft`'s deserializer expects for every library.
fn convert_fabric_library(lib: &serde_json::Value) -> Option<serde_json::Value> {
    let name = lib.get("name")?.as_str()?;
    let repo = lib.get("url")?.as_str()?;
    let path = maven_coordinate_to_path(name)?;
    let url = format!("{}/{path}", repo.trim_end_matches('/'));

    Some(serde_json::json!({
        "name": name,
        "downloads": {
            "artifact": {
                "path": path,
                "url": url,
                // Fabric's meta API doesn't publish a size/sha1 for
                // these the way Mojang's own metadata does.
                // `download_file` only warns (never fails) on a size
                // mismatch, so 0 here just disables that sanity check
                // for Fabric's own libraries.
                "size": 0
            }
        },
        "rules": []
    }))
}

/// Maps `group:artifact:version[:classifier]` to the standard repository path.
/// Missing required components return `None`; components after the optional
/// classifier are ignored because loader profiles use this four-part maximum.
fn maven_coordinate_to_path(coordinate: &str) -> Option<String> {
    // "group.id:artifact:version[:classifier]" ->
    // "group/id/artifact/version/artifact-version[-classifier].jar"
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
    let libs_dir = paths.libraries_dir();
    fs::create_dir_all(&libs_dir)?;

    let Some(libraries) = profile["libraries"].as_array() else {
        return Ok(());
    };

    for lib in libraries {
        let (Some(name), Some(repo)) = (
            lib.get("name").and_then(|v| v.as_str()),
            lib.get("url").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let Some(path) = maven_coordinate_to_path(name) else {
            continue;
        };

        let dest = libs_dir.join(&path);
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let url = format!("{}/{path}", repo.trim_end_matches('/'));
        minecraft::download_file(client, &url, &dest, None)?;
        println!("  fabric library: {name}");
    }

    Ok(())
}
