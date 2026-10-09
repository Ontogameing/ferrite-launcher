//! Shared loader metadata and Maven-library operations. Backend policy stays at call sites.

use crate::core::paths::AppPaths;
use crate::minecraft::{self, Result};
use reqwest::blocking::Client;
use std::fs;

/// Preserve coordinate parsing, including optional classifiers and ignored trailing fields.
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

/// Converts a profile library into Mojang's artifact shape; incomplete entries are skipped.
pub(super) fn convert_profile_library(lib: &serde_json::Value) -> Option<serde_json::Value> {
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
                // Profile APIs omit sizes; zero preserves the existing warning-only check.
                "size": 0
            }
        },
        "rules": []
    }))
}

/// Populate the cache with the same path checks, skip behavior and diagnostic label.
pub(super) fn download_profile_libraries(
    paths: &AppPaths,
    client: &Client,
    profile: &serde_json::Value,
    loader_label: &str,
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

        let dest = minecraft::metadata_path(&libs_dir, &path)?;
        if dest.exists() {
            continue;
        }
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }

        let url = format!("{}/{path}", repo.trim_end_matches('/'));
        minecraft::download_file(client, &url, &dest, None)?;
        println!("  {loader_label} library: {name}");
    }

    Ok(())
}

pub(super) fn merge_installer_metadata(
    composite_id: &str,
    vanilla: &serde_json::Value,
    loader: &serde_json::Value,
    default_repository: &str,
) -> serde_json::Value {
    let mut merged = vanilla.clone();
    merged["id"] = serde_json::Value::String(composite_id.to_string());
    merged.as_object_mut().map(|o| o.remove("inheritsFrom"));

    if let Some(main_class) = loader.get("mainClass") {
        merged["mainClass"] = main_class.clone();
    }

    let mut libraries = vanilla["libraries"].as_array().cloned().unwrap_or_default();
    if let Some(loader_libs) = loader["libraries"].as_array() {
        for lib in loader_libs {
            libraries.push(normalize_library(lib, default_repository));
        }
    }
    merged["libraries"] = serde_json::Value::Array(libraries);

    append_args(&mut merged, vanilla, loader, "game");
    append_args(&mut merged, vanilla, loader, "jvm");

    merged
}

/// Appends one argument category without creating an empty array when neither
/// parent contributes values.
fn append_args(
    merged: &mut serde_json::Value,
    vanilla: &serde_json::Value,
    loader: &serde_json::Value,
    kind: &str,
) {
    let mut args = vanilla["arguments"][kind]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if let Some(extra) = loader
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
fn normalize_library(lib: &serde_json::Value, default_repository: &str) -> serde_json::Value {
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
        .unwrap_or(default_repository);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn maven_paths_preserve_coordinate_compatibility() {
        for (coordinate, expected) in [
            (
                "org.example:loader:2",
                Some("org/example/loader/2/loader-2.jar"),
            ),
            (
                "org.example:loader:2:universal:ignored",
                Some("org/example/loader/2/loader-2-universal.jar"),
            ),
            ("org.example:loader", None),
            ("org.example", None),
            ("g:a:", Some("g/a//a-.jar")),
        ] {
            assert_eq!(maven_coordinate_to_path(coordinate).as_deref(), expected);
        }
    }

    #[test]
    fn profile_library_conversion_preserves_skip_and_artifact_behavior() {
        for library in [
            json!({}),
            json!({"name": "g:a:1"}),
            json!({"name": "invalid", "url": "repo"}),
        ] {
            assert!(convert_profile_library(&library).is_none());
        }
        assert_eq!(
            convert_profile_library(&json!({"name": "g:a:1", "url": "https://repo.invalid///"})),
            Some(json!({
                "name": "g:a:1", "downloads": {"artifact": {
                    "path": "g/a/1/a-1.jar", "url": "https://repo.invalid/g/a/1/a-1.jar", "size": 0
                }}, "rules": []
            }))
        );
    }

    #[test]
    fn installer_merge_preserves_missing_arguments_and_unparseable_libraries() {
        let vanilla = json!({"id": "vanilla", "inheritsFrom": "parent", "mainClass": "vanilla.Main", "libraries": []});
        let libraries = json!([{"name": "invalid"}, {"url": "repo"}]);
        let loader = json!({"libraries": libraries, "arguments": {"game": [], "jvm": []}});
        let merged =
            merge_installer_metadata("composite", &vanilla, &loader, "https://default.invalid/");
        assert_eq!(merged["id"], "composite");
        assert_eq!(merged["mainClass"], "vanilla.Main");
        assert!(merged.get("inheritsFrom").is_none());
        assert!(merged.get("arguments").is_none());
        assert_eq!(merged["libraries"], libraries);
    }

    #[test]
    fn installer_libraries_keep_explicit_repositories_and_rules() {
        let rules = json!([{"action": "allow"}]);
        let library = json!({"name": "g:a:1", "url": "https://custom.invalid/", "rules": rules});
        let normalized = normalize_library(&library, "https://default.invalid/");
        assert_eq!(
            normalized["downloads"]["artifact"]["url"],
            "https://custom.invalid/g/a/1/a-1.jar"
        );
        assert_eq!(normalized["rules"], rules);
        let prebuilt =
            json!({"name": "g:a:1", "downloads": {"artifact": {"path": null}}, "rules": null});
        assert_eq!(
            normalize_library(&prebuilt, "https://default.invalid/"),
            prebuilt
        );
    }
}
