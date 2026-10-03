//! # Mod-loader installation/launch dispatch for Ferrite Launcher.
//!
//! `app.rs` should call into this module for install/launch instead of
//! `crate::minecraft` directly, so loader selection lives in one place.
//! Each loader gets its own submodule; this file matches on [`ModLoader`],
//! delegates installation and local-id discovery, and funnels every launch back
//! through `crate::minecraft` for argument construction and process management.
//! Vanilla is the identity case: its launch id is the requested Minecraft id.
//!
//! Loader installs share the relative `minecraft/` storage tree with vanilla.
//! Each loader writes a small marker beside the vanilla version metadata that
//! records the exact synthetic version id selected at install time. Launch and
//! status checks are therefore local and deterministic: they do not contact a
//! loader API or silently switch to a newer published build.
//!
//! # How a loader installs (see `fabric.rs` for the concrete example)
//!
//! `crate::minecraft` has no idea mod loaders exist, and it doesn't need
//! to: a loader installs by building an ordinary *vanilla-shaped*
//! synthetic version (its own id, its own metadata JSON, its own copy
//! of `client.jar`) out of the already-installed vanilla version plus
//! whatever the loader adds, and then just calls
//! `crate::minecraft::launch_authenticated` / `is_version_installed` on that
//! synthetic id like it was any other release. That's what keeps this
//! module — and `minecraft.rs` — from needing to know anything
//! loader-specific about classpath building, argument resolution, or
//! process spawning.
//!
//! # Adding another loader
//!
//! A new backend should expose the same operations used by the dispatch functions below:
//! installation (including an optional exact loader version), discovery of the installed
//! synthetic Minecraft id, and discovery of the installed loader version. Add the loader
//! to [`ModLoader`] and route each install, launch, and status function to that backend.
//!
//! The backend may consume a metadata API, as Fabric and Quilt do, or normalize the output
//! of a Java installer, as Forge and NeoForge do. In either case, its durable result must
//! be the same vanilla-shaped version directory and marker contract expected here.

mod fabric;
mod forge;
mod neoforge;
mod quilt;

use crate::minecraft::{self, Result};
use ferrite_launcher::core::paths::AppPaths;
use std::path::Path;
use std::process::Command;

/// Builds `java -jar <installer> --installClient <minecraft dir>` for the official
/// Forge/NeoForge installers.
///
/// Every path is its own argv element (no shell, no string concatenation), so paths
/// containing spaces, such as `...\Ferrite\Ferrite Launcher\data\minecraft` on
/// Windows, reach Java intact.
pub(crate) fn installer_command(installer: &Path, minecraft_dir: &Path) -> Command {
    let mut command = Command::new("java");
    command
        .arg("-jar")
        .arg(installer)
        .arg("--installClient")
        .arg(minecraft_dir);
    command
}

/// Loader implementation to install, inspect, or launch.
///
/// This value is `Copy`, so dispatch functions take it by value without moving
/// any heap-owned state from their callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModLoader {
    /// Unmodified Mojang metadata and client.
    Vanilla,
    /// Fabric profile metadata layered over vanilla.
    Fabric,
    /// Forge's client installer output normalized over vanilla.
    Forge,
    /// NeoForge's client installer output normalized over vanilla.
    NeoForge,
    /// Quilt profile metadata layered over vanilla.
    Quilt,
}

impl ModLoader {
    /// Every variant, in the order the GUI's combo box already lists
    /// them.
    pub const ALL: [ModLoader; 5] = [
        ModLoader::Vanilla,
        ModLoader::Forge,
        ModLoader::Fabric,
        ModLoader::Quilt,
        ModLoader::NeoForge,
    ];

    /// The exact string the GUI's "Mod Loader" combo box uses for this
    /// loader.
    pub fn label(self) -> &'static str {
        match self {
            ModLoader::Vanilla => "Vanilla",
            ModLoader::Fabric => "Fabric",
            ModLoader::Forge => "Forge",
            ModLoader::NeoForge => "NeoForge",
            ModLoader::Quilt => "Quilt",
        }
    }

    /// Text for loader pickers. Only the shown text differs from [`Self::label`];
    /// the stored value stays `label()` so saved data doesn't change.
    pub fn picker_label(self) -> &'static str {
        match self {
            ModLoader::Quilt => "Quilt (experimental)",
            other => other.label(),
        }
    }

    /// Parses the exact, case-sensitive display label used by [`Self::label`].
    /// Unknown labels return `None` rather than defaulting to vanilla.
    pub fn from_label(label: &str) -> Option<ModLoader> {
        ModLoader::ALL.into_iter().find(|l| l.label() == label)
    }
}

/// Installs `mc_version` under the given loader. For `Vanilla` this is
/// exactly `minecraft::install_version`; other loaders install the
/// vanilla version first (if it isn't already) and then layer their own
/// libraries/main-class on top of it.
pub fn install(paths: &AppPaths, mc_version: &str, loader: ModLoader) -> Result<()> {
    match loader {
        ModLoader::Vanilla => minecraft::install_version(paths, mc_version),
        ModLoader::Fabric => fabric::install(paths, mc_version),
        ModLoader::Forge => forge::install(paths, mc_version),
        ModLoader::NeoForge => neoforge::install(paths, mc_version),
        ModLoader::Quilt => quilt::install(paths, mc_version),
    }
}

/// Installs an exact loader version when a pack manifest pins one.
///
/// `None` delegates to [`install`] and lets the selected loader discover its
/// preferred current build. Vanilla ignores a supplied loader version because
/// Mojang's version id already identifies the complete install.
pub fn install_version(
    paths: &AppPaths,
    mc_version: &str,
    loader: ModLoader,
    loader_version: Option<&str>,
) -> Result<()> {
    let Some(loader_version) = loader_version else {
        return install(paths, mc_version, loader);
    };
    match loader {
        ModLoader::Vanilla => minecraft::install_version(paths, mc_version),
        ModLoader::Fabric => fabric::install_version(paths, mc_version, Some(loader_version)),
        ModLoader::Forge => forge::install_version(paths, mc_version, Some(loader_version)),
        ModLoader::NeoForge => neoforge::install_version(paths, mc_version, Some(loader_version)),
        ModLoader::Quilt => quilt::install_version(paths, mc_version, Some(loader_version)),
    }
}

/// Explicit offline compatibility wrapper using placeholder credentials.
/// Account-aware callers should use `launch_authenticated`.
/// Launches `mc_version` under the given loader using the shared Minecraft
/// directory as the game's directory, preserving the original behavior.
pub fn launch(paths: &AppPaths, mc_version: &str, loader: ModLoader) -> Result<()> {
    launch_in_directory(paths, mc_version, loader, paths.storage_root())
}

/// Explicit offline compatibility wrapper; use `launch_authenticated` for accounts.
/// Launches `mc_version` under the given loader with per-instance saves,
/// configuration, mods, and logs rooted at `game_dir`. Installed versions,
/// libraries, assets, and natives continue to come from shared storage.
pub fn launch_in_directory(
    paths: &AppPaths,
    mc_version: &str,
    loader: ModLoader,
    game_dir: &Path,
) -> Result<()> {
    let version = prepare_launch_version(paths, mc_version, loader)?;
    minecraft::launch_version_in_directory(paths, &version, game_dir)
}

/// Offline launch with the configured maximum Java heap size.
pub fn launch_in_directory_with_memory(
    paths: &AppPaths,
    mc_version: &str,
    loader: ModLoader,
    game_dir: &Path,
    memory_mb: u32,
) -> Result<()> {
    let version = prepare_launch_version(paths, mc_version, loader)?;
    minecraft::launch_version_in_directory_with_memory(paths, &version, game_dir, memory_mb)
}

/// Launches with an authenticated Microsoft account and per-instance game data.
/// Expired sessions fail before loader preparation; there is no offline fallback.
/// Installed versions, libraries, assets, and natives remain in shared storage.
pub fn launch_authenticated(
    paths: &AppPaths,
    mc_version: &str,
    loader: ModLoader,
    game_dir: &Path,
    account: &crate::auth::Account,
) -> Result<()> {
    if account.is_expired() {
        return Err(minecraft::FerriteError::AuthenticationExpired);
    }
    let version = prepare_launch_version(paths, mc_version, loader)?;
    minecraft::launch_authenticated(paths, &version, game_dir, account)
}

/// Authenticated launch with the configured maximum Java heap size.
pub fn launch_authenticated_with_memory(
    paths: &AppPaths,
    mc_version: &str,
    loader: ModLoader,
    game_dir: &Path,
    account: &crate::auth::Account,
    memory_mb: u32,
) -> Result<()> {
    if account.is_expired() {
        return Err(minecraft::FerriteError::AuthenticationExpired);
    }
    let version = prepare_launch_version(paths, mc_version, loader)?;
    minecraft::launch_authenticated_with_memory(paths, &version, game_dir, account, memory_mb)
}

/// Resolves a UI-level `(Minecraft, loader)` choice to the installed version id
/// understood by `minecraft.rs`. Loader lookup errors propagate to launch; after
/// lookup, vanilla natives are recopied into the synthetic native directory to
/// repair missing workdirs before every launch.
fn prepare_launch_version(paths: &AppPaths, mc_version: &str, loader: ModLoader) -> Result<String> {
    let composite_id = match loader {
        ModLoader::Vanilla => return Ok(mc_version.to_string()),
        ModLoader::Fabric => fabric::installed_composite_id(paths, mc_version)?,
        ModLoader::Forge => forge::installed_composite_id(paths, mc_version)?,
        ModLoader::NeoForge => neoforge::installed_composite_id(paths, mc_version)?,
        ModLoader::Quilt => quilt::installed_composite_id(paths, mc_version)?,
    };
    minecraft::copy_natives(paths, mc_version, &composite_id)?;
    Ok(composite_id)
}

/// Returns whether the selected local version has both metadata and `client.jar`.
///
/// For loaders, a missing/unreadable marker and any loader-id lookup error are
/// deliberately collapsed to `false`; this status probe never performs network
/// I/O and cannot distinguish a partial install from no install.
pub fn is_installed(paths: &AppPaths, mc_version: &str, loader: ModLoader) -> bool {
    match loader {
        ModLoader::Vanilla => minecraft::is_version_installed(paths, mc_version),
        ModLoader::Fabric => fabric::installed_composite_id(paths, mc_version)
            .map(|id| minecraft::is_version_installed(paths, &id))
            .unwrap_or(false),
        ModLoader::Forge => forge::installed_composite_id(paths, mc_version)
            .map(|id| minecraft::is_version_installed(paths, &id))
            .unwrap_or(false),
        ModLoader::NeoForge => neoforge::installed_composite_id(paths, mc_version)
            .map(|id| minecraft::is_version_installed(paths, &id))
            .unwrap_or(false),
        ModLoader::Quilt => quilt::installed_composite_id(paths, mc_version)
            .map(|id| minecraft::is_version_installed(paths, &id))
            .unwrap_or(false),
    }
}

/// Reads the exact installed loader version from the synthetic version metadata.
///
/// This is used for portable pack manifests and never performs a network request.
/// Missing markers/files, malformed JSON, absent coordinates, and vanilla all
/// return `None`; this best-effort query intentionally does not expose errors.
pub fn installed_loader_version(
    paths: &AppPaths,
    mc_version: &str,
    loader: ModLoader,
) -> Option<String> {
    let composite_id = match loader {
        ModLoader::Vanilla => return None,
        ModLoader::Fabric => fabric::installed_composite_id(paths, mc_version).ok()?,
        ModLoader::Forge => forge::installed_composite_id(paths, mc_version).ok()?,
        ModLoader::NeoForge => neoforge::installed_composite_id(paths, mc_version).ok()?,
        ModLoader::Quilt => quilt::installed_composite_id(paths, mc_version).ok()?,
    };
    let metadata = std::fs::read_to_string(
        paths
            .version_dir(&composite_id)
            .join(format!("{composite_id}.json")),
    )
    .ok()?;
    let metadata: serde_json::Value = serde_json::from_str(&metadata).ok()?;
    loader_version_from_metadata(mc_version, loader, &metadata)
}

/// Finds the first recognized loader Maven coordinate. Classifiers are removed,
/// and Forge-style versions prefixed with `<minecraft>-` are normalized to the
/// loader-only version used in portable manifests.
fn loader_version_from_metadata(
    mc_version: &str,
    loader: ModLoader,
    metadata: &serde_json::Value,
) -> Option<String> {
    let coordinates: &[&str] = match loader {
        ModLoader::Vanilla => return None,
        ModLoader::Fabric => &["net.fabricmc:fabric-loader:"],
        ModLoader::Forge => &["net.minecraftforge:forge:"],
        ModLoader::NeoForge => &["net.neoforged:neoforge:", "net.neoforged:forge:"],
        ModLoader::Quilt => &["org.quiltmc:quilt-loader:"],
    };
    metadata
        .get("libraries")?
        .as_array()?
        .iter()
        .filter_map(|library| library.get("name").and_then(|name| name.as_str()))
        .find_map(|name| {
            coordinates.iter().find_map(|prefix| {
                name.strip_prefix(prefix).map(|version| {
                    let version = version.split(':').next().unwrap_or(version);
                    version
                        .strip_prefix(&format!("{mc_version}-"))
                        .unwrap_or(version)
                        .to_owned()
                })
            })
        })
        .filter(|version| !version.is_empty())
}

#[cfg(test)]
mod tests {
    #[test]
    fn quilt_picker_label_is_marked_experimental_but_stored_label_is_unchanged() {
        use super::ModLoader;
        assert_eq!(ModLoader::Quilt.picker_label(), "Quilt (experimental)");
        assert_eq!(ModLoader::Quilt.label(), "Quilt");
        assert_eq!(ModLoader::from_label("Quilt"), Some(ModLoader::Quilt));
        for loader in ModLoader::ALL {
            if loader != ModLoader::Quilt {
                assert_eq!(loader.picker_label(), loader.label());
            }
        }
    }

    #[test]
    fn installer_command_keeps_paths_with_spaces_as_single_arguments() {
        let installer = Path::new("/tmp/Ferrite Launcher/cache/downloads/forge installer.jar");
        let minecraft = Path::new("/tmp/Ferrite Launcher/data/minecraft");
        let command = super::installer_command(installer, minecraft);
        assert_eq!(command.get_program(), "java");
        let args: Vec<&std::ffi::OsStr> = command.get_args().collect();
        assert_eq!(
            args,
            [
                std::ffi::OsStr::new("-jar"),
                installer.as_os_str(),
                std::ffi::OsStr::new("--installClient"),
                minecraft.as_os_str(),
            ]
        );
    }

    use super::*;

    #[test]
    fn loader_versions_are_read_from_library_coordinates() {
        let forge = serde_json::json!({
            "libraries": [{ "name": "net.minecraftforge:forge:1.21.1-52.0.4:universal" }]
        });
        assert_eq!(
            loader_version_from_metadata("1.21.1", ModLoader::Forge, &forge).as_deref(),
            Some("52.0.4")
        );
        let fabric = serde_json::json!({
            "libraries": [{ "name": "net.fabricmc:fabric-loader:0.16.10" }]
        });
        assert_eq!(
            loader_version_from_metadata("1.21.1", ModLoader::Fabric, &fabric).as_deref(),
            Some("0.16.10")
        );
    }
}
