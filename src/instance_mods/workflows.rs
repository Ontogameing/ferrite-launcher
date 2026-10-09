//! Blocking mod workflows. The frontend schedules work and renders its result.

use super::InstalledMod;
use crate::core::{instances::InstanceProfile, paths::AppPaths};

pub struct LocalResult {
    pub target: InstanceProfile,
    pub message: String,
    pub result: Result<Vec<InstalledMod>, String>,
}

pub fn target_exists(profiles: &[InstanceProfile], target: &InstanceProfile) -> bool {
    profiles
        .iter()
        .any(|profile| profile.directory() == target.directory())
}

pub fn accepts_result(
    profiles: &[InstanceProfile],
    current: Option<&InstanceProfile>,
    target: &InstanceProfile,
) -> bool {
    target_exists(profiles, target)
        && current.is_some_and(|current| current.directory() == target.directory())
}

pub fn can_install(
    profiles: &[InstanceProfile],
    target: &InstanceProfile,
    is_mod: bool,
    running: bool,
) -> bool {
    is_mod
        && !running
        && target_exists(profiles, target)
        && matches!(
            target.loader.to_ascii_lowercase().as_str(),
            "fabric" | "forge" | "neoforge" | "quilt"
        )
}

pub fn install(
    paths: &AppPaths,
    target: InstanceProfile,
    project_id: &str,
    title: &str,
) -> LocalResult {
    let game_dir = target.game_dir(paths);
    let operation = if crate::minecraft::is_running() {
        Err("Stop Minecraft before installing mods.".into())
    } else {
        crate::modrinth::install(
            project_id,
            &target.version,
            &target.loader.to_ascii_lowercase(),
            &game_dir,
        )
        .map(|paths| paths.len())
    };
    let message = match operation {
        Ok(count) => format!("Installed {title} ({count} files) into '{}'.", target.name),
        Err(error) => format!("Install into '{}' failed: {error}", target.name),
    };
    LocalResult {
        result: super::list(&game_dir),
        target,
        message,
    }
}

/// Refreshes after both successful and failed mutations, preserving diagnostic order.
pub fn manage(
    paths: &AppPaths,
    target: InstanceProfile,
    action: Option<(String, Option<bool>)>,
) -> LocalResult {
    let game_dir = target.game_dir(paths);
    let operation = if crate::minecraft::is_running() {
        Err("Stop Minecraft before managing mods.".into())
    } else {
        match action {
            Some((filename, Some(enabled))) => {
                super::set_enabled(&game_dir, &filename, enabled).map(|_| ())
            }
            Some((filename, None)) => super::uninstall(&game_dir, &filename),
            None => Ok(()),
        }
    };
    let message = match operation {
        Ok(()) => format!("Refreshed mods for '{}'.", target.name),
        Err(error) => format!("Mods for '{}': {error}", target.name),
    };
    LocalResult {
        result: super::list(&game_dir),
        target,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_acceptance_uses_original_target_and_live_membership() {
        let a = InstanceProfile::new("A".into(), "1.20.1".into(), "Fabric".into(), &[]);
        let b = InstanceProfile::new("B".into(), "1.20.1".into(), "Fabric".into(), &[]);
        let profiles = vec![a.clone(), b.clone()];
        assert!(accepts_result(&profiles, Some(&a), &a));
        assert!(!accepts_result(&profiles, Some(&b), &a));
        assert!(!accepts_result(&[], Some(&a), &a));
        assert!(can_install(&profiles, &a, true, false));
        assert!(!can_install(&profiles, &a, true, true));
        assert!(!can_install(&profiles, &a, false, false));
    }
}
