//! # Forge loader support.
//!
//! Forge does **not** publish a ready-to-use version JSON the way Fabric
//! and Quilt do. What Maven actually ships is an *installer JAR*:
//!
//! ```text
//! https://maven.minecraftforge.net/net/minecraftforge/forge/{MC}-{FORGE}/
//!     forge-{MC}-{FORGE}-installer.jar
//! ```
//!
//! Running
//!
//! ```text
//! java -jar forge-…-installer.jar --installClient <minecraft/>
//! ```
//!
//! is how every real launcher installs Forge. The installer:
//!
//! 1. Writes `minecraft/versions/<id>/<id>.json` (usually with
//!    `"inheritsFrom": "<vanilla mc version>"`).
//! 2. Drops Forge's extra jars under `minecraft/libraries/`.
//! 3. Optionally writes `minecraft/versions/<id>/<id>.jar`.
//!
//! After that, this module still has one extra job: `crate::minecraft`
//! does **not** understand `inheritsFrom`. We read the installer's JSON,
//! merge it with the already-installed vanilla metadata (libraries,
//! `mainClass`, game **and** JVM args), write a vanilla-shaped JSON
//! back into the same version directory, and make sure `client.jar`
//! exists so `is_version_installed` / `launch_version` work unchanged.
//!
//! A marker file (`forge-loader.txt`) records the synthetic id so
//! `launch` / `is_installed` don't have to guess which folder the
//! installer created. It is written only after metadata normalization, client
//! selection, and native copying succeed. Installer output and Maven artifacts
//! live in the shared relative `minecraft/` tree; a failed attempt may leave
//! partial files there, but without a marker dispatch treats Forge as uninstalled.

use crate::minecraft::{self, FerriteError, Result};
use ferrite_launcher::core::paths::AppPaths;
use reqwest::blocking::Client;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use zip::{ZipArchive, ZipWriter};

const FORGE_MAVEN: &str = "https://maven.minecraftforge.net";

/// Installs the latest available Forge build for `mc_version`.
pub fn install(paths: &AppPaths, mc_version: &str) -> Result<()> {
    install_version(paths, mc_version, None)
}

/// Installs a requested Forge version, or the latest Maven-listed build.
///
/// The operation first ensures vanilla is complete, supplies compatibility files
/// expected by Forge's official installer, downloads and runs that installer,
/// discovers the version directory it produced, flattens inherited metadata,
/// chooses a launchable client jar, copies natives, and finally writes the marker.
/// A legacy `:universal` suffix on a pinned version is accepted and removed.
/// Errors abort immediately; installer deletion is best-effort and only attempted
/// after the installer returns successfully from [`run_installer`].
pub fn install_version(paths: &AppPaths, mc_version: &str, requested: Option<&str>) -> Result<()> {
    minecraft::install_version(paths, mc_version)?;

    let client = Client::new();
    let forge_version = match requested.map(str::trim).filter(|value| !value.is_empty()) {
        // Older Ferrite exports accidentally retained this Maven classifier.
        Some(version) => version
            .strip_suffix(":universal")
            .unwrap_or(version)
            .to_owned(),
        None => latest_forge_version(&client, mc_version)?,
    };
    println!("Installing Forge {forge_version} for Minecraft {mc_version}");

    // The official installer looks for `versions/<mc>/<mc>.jar`. Our
    // vanilla install writes `client.jar` instead, so give it a copy
    // under the name it expects.
    let vanilla_dir = paths.version_dir(mc_version);
    let vanilla_client = vanilla_dir.join("client.jar");
    let vanilla_named = vanilla_dir.join(format!("{mc_version}.jar"));
    if !vanilla_named.exists() {
        fs::copy(&vanilla_client, &vanilla_named)?;
    }

    let coord = format!("{mc_version}-{forge_version}");
    let installer_url =
        format!("{FORGE_MAVEN}/net/minecraftforge/forge/{coord}/forge-{coord}-installer.jar");
    let installer_path = paths
        .downloads_dir()
        .join(format!("forge-{coord}-installer.jar"));
    if let Some(parent) = installer_path.parent() {
        fs::create_dir_all(parent)?;
    }
    println!("Downloading Forge installer...");
    minecraft::download_file(&client, &installer_url, &installer_path, None)?;

    // The installer refuses to run unless a vanilla-style
    // launcher_profiles.json already exists in the target directory.
    ensure_launcher_profiles(paths.storage_root())?;

    println!("Running Forge installer (this can take a while)...");
    run_installer(&installer_path, paths.storage_root())?;
    let _ = fs::remove_file(&installer_path);

    let composite_id = find_installed_forge_id(paths, mc_version, &forge_version)?;
    println!("Forge installer created version id `{composite_id}`");

    let composite_dir = paths.version_dir(&composite_id);
    let json_path = composite_dir.join(format!("{composite_id}.json"));
    let forge_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(&json_path)?)?;

    let vanilla_json: serde_json::Value = serde_json::from_str(&fs::read_to_string(
        vanilla_dir.join(format!("{mc_version}.json")),
    )?)?;

    let merged = merge_inherited_metadata(&composite_id, &vanilla_json, &forge_json);
    fs::write(&json_path, serde_json::to_string_pretty(&merged)?)?;

    ensure_client_jar(&composite_dir, &composite_id, &vanilla_client)?;
    minecraft::copy_natives(paths, mc_version, &composite_id)?;

    fs::write(marker_path(paths, mc_version), &composite_id)?;
    println!("Forge {forge_version} installed for Minecraft {mc_version}.");
    Ok(())
}

/// The synthetic version id of the merged vanilla+Forge install currently
/// recorded for `mc_version`.
pub fn installed_composite_id(paths: &AppPaths, mc_version: &str) -> Result<String> {
    fs::read_to_string(marker_path(paths, mc_version))
        .map_err(|_| FerriteError::LoaderNotInstalled(mc_version.to_string()))
}

fn marker_path(paths: &AppPaths, mc_version: &str) -> PathBuf {
    paths.version_dir(mc_version).join("forge-loader.txt")
}

// ---------------------------------------------------------------------
// Version discovery
// ---------------------------------------------------------------------

/// Reads Forge's Maven metadata and selects the last exact-Minecraft match,
/// relying on repository order to place newer builds later. No match becomes
/// `LoaderVersionUnavailable`; request errors propagate.
fn latest_forge_version(client: &Client, mc_version: &str) -> Result<String> {
    let meta_url = format!("{FORGE_MAVEN}/net/minecraftforge/forge/maven-metadata.xml");
    let text = client.get(&meta_url).send()?.error_for_status()?.text()?;
    extract_latest_forge_from_metadata(&text, mc_version)
        .ok_or_else(|| FerriteError::LoaderVersionUnavailable(mc_version.to_string()))
}

/// Each `<version>` entry is `MC-FORGE`, e.g. `1.21.1-52.1.0`. Only
/// entries whose Minecraft part is *exactly* `mc_version` count, so
/// looking up `1.21` does not pick `1.21.1`.
fn extract_latest_forge_from_metadata(xml: &str, mc_version: &str) -> Option<String> {
    let prefix = format!("{mc_version}-");
    xml.split("<version>")
        .filter_map(|chunk| {
            let end = chunk.find("</version>")?;
            let full = &chunk[..end];
            if full.split('-').next() == Some(mc_version) && full.starts_with(&prefix) {
                Some(full[prefix.len()..].to_string())
            } else {
                None
            }
        })
        .last()
}

// ---------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------

/// Forge's installer looks for `launcher_profiles.json` and bails with
/// "you need to run the launcher first!" if it isn't there. A dummy
/// empty profile file is enough.
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

/// Runs the official installer synchronously against a canonicalized Minecraft
/// root. Before execution it may create a patched temporary JAR that omits
/// processor output hashes; that temporary is removed after Java exits, even for
/// a non-zero status. Spawn/I/O failures propagate, while a non-zero exit captures
/// status, stdout, and stderr in `InstallerFailed`.
fn run_installer(installer: &Path, minecraft_dir: &Path) -> Result<()> {
    fs::create_dir_all(minecraft_dir)?;
    let minecraft_abs = fs::canonicalize(minecraft_dir)?;
    let installer_abs = fs::canonicalize(installer)?;
    // Processor JAR hashes fail on zlib-ng (Installer#80). Strip them so
    // the official installer still runs.
    let patched = strip_processor_output_hashes(&installer_abs)?;

    println!("Forge installer command:");
    println!(
        "  java -jar \"{}\" --installClient \"{}\"",
        patched.display(),
        minecraft_abs.display()
    );

    let output = Command::new("java")
        .arg("-jar")
        .arg(&patched)
        .arg("--installClient")
        .arg(&minecraft_abs)
        .output()?;

    if patched != installer_abs {
        let _ = fs::remove_file(&patched);
    }

    println!("Forge installer exit status: {:?}", output.status);
    println!(
        "Forge installer stdout:\n{}",
        String::from_utf8_lossy(&output.stdout)
    );
    println!(
        "Forge installer stderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    if !output.status.success() {
        return Err(FerriteError::InstallerFailed(format!(
            "exit={:?} stdout={:?} stderr={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    Ok(())
}

/// Copies `installer` with processor `outputs` hashes removed. Forge hashes
/// the compressed JAR bytes from processors such as ForgeAutoRenamingTool;
/// zlib-ng (used as system zlib on several Linux distros) produces different
/// compressed bytes for the same class files, so the official hashes fail
/// even though the contents are valid. See MinecraftForge/Installer#80.
///
/// If no `install_profile.json` exists, the original path is returned unchanged.
/// Otherwise entries are raw-copied, the profile is rewritten, and stale JAR
/// signatures are omitted because changing a signed entry invalidates them.
fn strip_processor_output_hashes(installer: &Path) -> Result<PathBuf> {
    let patched = installer.with_file_name(format!(
        "{}-noshahash.jar",
        installer.file_stem().unwrap_or_default().to_string_lossy()
    ));

    let mut archive = ZipArchive::new(fs::File::open(installer)?)?;
    let profile_bytes = match archive.by_name("install_profile.json") {
        Ok(mut file) => {
            let mut json = String::new();
            file.read_to_string(&mut json)?;
            Some(strip_outputs_from_profile(&json)?)
        }
        Err(zip::result::ZipError::FileNotFound) => None,
        Err(e) => return Err(e.into()),
    };

    let Some(profile_bytes) = profile_bytes else {
        return Ok(installer.to_path_buf());
    };

    let mut writer = ZipWriter::new(fs::File::create(&patched)?);
    for i in 0..archive.len() {
        let file = archive.by_index(i)?;
        let name = file.name().to_string();
        if is_jar_signature(&name) || name == "install_profile.json" {
            continue;
        }
        writer.raw_copy_file(file)?;
    }
    writer.start_file(
        "install_profile.json",
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Deflated),
    )?;
    writer.write_all(&profile_bytes)?;
    writer.finish()?;
    Ok(patched)
}

fn strip_outputs_from_profile(json: &str) -> Result<Vec<u8>> {
    let mut profile: serde_json::Value = serde_json::from_str(json)?;
    if let Some(processors) = profile.get_mut("processors").and_then(|p| p.as_array_mut()) {
        for processor in processors {
            if let Some(obj) = processor.as_object_mut() {
                obj.remove("outputs");
            }
        }
    }
    Ok(serde_json::to_vec(&profile)?)
}

fn is_jar_signature(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper.starts_with("META-INF/")
        && (upper.ends_with(".SF")
            || upper.ends_with(".RSA")
            || upper.ends_with(".DSA")
            || upper.ends_with(".EC"))
}

/// Locates the version folder the installer just created.
///
/// Known modern and legacy names are checked first. As a compatibility fallback,
/// every Forge-named version directory is scanned for JSON mentioning both the
/// requested build and either the parent Minecraft id or an id containing it.
/// Unreadable directories/files are skipped; exhausting the search is reported
/// as `InstallerFailed` because Java had already claimed installation success.
fn find_installed_forge_id(
    paths: &AppPaths,
    mc_version: &str,
    forge_version: &str,
) -> Result<String> {
    let candidates = [
        format!("{mc_version}-forge-{forge_version}"),
        format!("{mc_version}-Forge{forge_version}"),
        format!("{mc_version}-forge-{mc_version}-{forge_version}"),
    ];
    for id in &candidates {
        if minecraft::is_version_installed(paths, id)
            || paths.version_dir(id).join(format!("{id}.json")).exists()
        {
            return Ok(id.clone());
        }
    }

    // Last resort: any versions/* directory whose JSON mentions this
    // Forge build and inherits this Minecraft version.
    let versions_root = paths.versions_dir();
    if let Ok(entries) = fs::read_dir(&versions_root) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.to_ascii_lowercase().contains("forge") {
                continue;
            }
            let json_path = entry.path().join(format!("{name}.json"));
            let Ok(text) = fs::read_to_string(json_path) else {
                continue;
            };
            if text.contains(forge_version)
                && (text.contains(&format!("\"inheritsFrom\": \"{mc_version}\""))
                    || name.contains(mc_version))
            {
                return Ok(name);
            }
        }
    }

    Err(FerriteError::InstallerFailed(format!(
        "Forge installer finished but no version folder was found for {mc_version}-{forge_version}"
    )))
}

/// Ensures the normalized layout has `client.jar`, preferring an existing client,
/// then the installer's `<composite-id>.jar`, then vanilla's client. Copies replace
/// only the missing destination and propagate filesystem errors.
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

/// Flattens Forge's inherited metadata into an owned vanilla-shaped document.
///
/// Vanilla is cloned as the base. The synthetic id, Forge main class, normalized
/// Forge libraries, and Forge game/JVM arguments are overlaid or appended in
/// source order; `inheritsFrom` is removed because `minecraft.rs` does not resolve
/// parent metadata. Missing optional Forge sections preserve vanilla behavior.
/// JVM arguments are essential to modern Forge's bootstrap launcher.
fn merge_inherited_metadata(
    composite_id: &str,
    vanilla: &serde_json::Value,
    forge: &serde_json::Value,
) -> serde_json::Value {
    let mut merged = vanilla.clone();
    merged["id"] = serde_json::Value::String(composite_id.to_string());
    merged.as_object_mut().map(|o| o.remove("inheritsFrom"));

    if let Some(main_class) = forge.get("mainClass") {
        merged["mainClass"] = main_class.clone();
    }

    let mut libraries = vanilla["libraries"].as_array().cloned().unwrap_or_default();
    if let Some(forge_libs) = forge["libraries"].as_array() {
        for lib in forge_libs {
            libraries.push(normalize_library(lib));
        }
    }
    merged["libraries"] = serde_json::Value::Array(libraries);

    append_args(&mut merged, vanilla, forge, "game");
    append_args(&mut merged, vanilla, forge, "jvm");

    merged
}

/// Appends one argument category without creating an empty array when neither
/// parent contributes values.
fn append_args(
    merged: &mut serde_json::Value,
    vanilla: &serde_json::Value,
    forge: &serde_json::Value,
    kind: &str,
) {
    let mut args = vanilla["arguments"][kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(extra) = forge
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

/// `crate::minecraft` requires every library to have
/// `downloads.artifact.{path,url,size}`. The installer JSON often only
/// has `name` (+ optional Maven `url`). Convert those into the vanilla
/// shape. Empty `url` is fine: the installer already dropped the jar
/// into `libraries/`.
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
        .unwrap_or("https://maven.minecraftforge.net/");
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

/// Converts `group:artifact:version[:classifier]` to its Maven cache path.
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
