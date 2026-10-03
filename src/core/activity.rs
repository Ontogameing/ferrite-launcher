//! Per-instance activity and the reasons an action is unavailable.
//!
//! The UI combines what it knows (is this instance's game running, which background
//! task holds which instance, which global tasks are busy, is the folder missing) into
//! an [`InstanceStatus`] and asks [`disabled_reason`] for each card action. The rules
//! follow the Stage 2 UI spec §2.2: the first matching condition wins, and a running
//! game blocks only its own card.

use std::collections::HashMap;

/// A long-running operation that owns one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyOperation {
    /// Version/loader change reinstalling files.
    Updating,
    /// This instance is the source of a duplicate in progress.
    Duplicating,
    /// Being moved to the trash or deleted.
    Deleting,
}

/// What an instance is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceActivity {
    Idle,
    Running,
    Busy(BusyOperation),
}

/// Launcher-wide tasks that block instance edits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GlobalBusy {
    pub pack_task: bool,
    pub mod_task: bool,
    pub creation_task: bool,
}

/// Everything needed to decide which actions a card offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceStatus {
    pub activity: InstanceActivity,
    /// The folder does not exist on disk (see `instances::folder_missing`).
    pub folder_missing: bool,
    pub global: GlobalBusy,
}

/// Card actions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstanceAction {
    Play,
    Mods,
    OpenFolder,
    /// Edit the display name.
    Rename,
    /// Edit the Minecraft version or loader.
    ChangeVersion,
    Duplicate,
    Export,
    /// Trash or permanent delete.
    Delete,
    /// "Remove from Ferrite, keep the files".
    RemoveFromList,
}

/// Returns why `action` is unavailable for the instance called `name`, or `None` if it
/// is allowed. Only the first applicable reason is returned.
pub fn disabled_reason(
    action: InstanceAction,
    name: &str,
    status: &InstanceStatus,
) -> Option<String> {
    use InstanceAction::*;
    let blocks = |actions: &[InstanceAction]| actions.contains(&action);
    match status.activity {
        InstanceActivity::Running => {
            if action == Play {
                return Some("Already running.".into());
            }
            if blocks(&[ChangeVersion, Duplicate, Export, Delete, RemoveFromList]) {
                return Some(format!("Close Minecraft first. {name} is running."));
            }
        }
        InstanceActivity::Busy(BusyOperation::Updating) => {
            if blocks(&[
                Play,
                Rename,
                ChangeVersion,
                Duplicate,
                Export,
                Delete,
                RemoveFromList,
            ]) {
                return Some("Ferrite is updating this instance.".into());
            }
        }
        InstanceActivity::Busy(BusyOperation::Duplicating) => {
            if blocks(&[
                Play,
                Rename,
                ChangeVersion,
                Duplicate,
                Export,
                Delete,
                RemoveFromList,
            ]) {
                return Some("Wait for the copy to finish.".into());
            }
        }
        InstanceActivity::Busy(BusyOperation::Deleting) => {
            if action != OpenFolder {
                return Some("Ferrite is removing this instance.".into());
            }
        }
        InstanceActivity::Idle => {}
    }
    let edits = [
        Rename,
        ChangeVersion,
        Duplicate,
        Export,
        Delete,
        RemoveFromList,
    ];
    if blocks(&edits) {
        if status.global.pack_task {
            return Some("Wait for the import or export to finish.".into());
        }
        if status.global.mod_task {
            return Some("Wait for mod changes to finish.".into());
        }
        if status.global.creation_task {
            return Some("Wait for the new instance to finish installing.".into());
        }
    }
    if status.folder_missing
        && blocks(&[
            Play,
            Mods,
            OpenFolder,
            Duplicate,
            Export,
            ChangeVersion,
            Delete,
        ])
    {
        // Delete (trash/permanent) has nothing to act on; RemoveFromList stays available.
        return Some("This instance's folder is missing.".into());
    }
    None
}

/// Tracks which instance (by folder name) is owned by which background operation.
/// Keys are compared case-insensitively, like folders.
#[derive(Debug, Clone, Default)]
pub struct ActivityTracker {
    busy: HashMap<String, BusyOperation>,
}

impl ActivityTracker {
    /// Marks `directory` busy. Returns `false` (and changes nothing) if it already is.
    pub fn begin(&mut self, directory: &str, operation: BusyOperation) -> bool {
        let key = directory.to_lowercase();
        if self.busy.contains_key(&key) {
            return false;
        }
        self.busy.insert(key, operation);
        true
    }

    /// Clears whatever operation held `directory`.
    pub fn end(&mut self, directory: &str) {
        self.busy.remove(&directory.to_lowercase());
    }

    pub fn busy(&self, directory: &str) -> Option<BusyOperation> {
        self.busy.get(&directory.to_lowercase()).copied()
    }

    /// Whether any instance is busy (e.g. to defer closing the window while deleting).
    pub fn any(&self, operation: BusyOperation) -> bool {
        self.busy.values().any(|busy| *busy == operation)
    }

    /// Combines the tracked operation with the running state. A busy operation wins
    /// over "running" because it was started first and owns the folder.
    pub fn activity(&self, directory: &str, running: bool) -> InstanceActivity {
        match self.busy(directory) {
            Some(operation) => InstanceActivity::Busy(operation),
            None if running => InstanceActivity::Running,
            None => InstanceActivity::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(activity: InstanceActivity) -> InstanceStatus {
        InstanceStatus {
            activity,
            folder_missing: false,
            global: GlobalBusy::default(),
        }
    }

    #[test]
    fn running_blocks_only_destructive_actions_on_its_card() {
        let running = status(InstanceActivity::Running);
        assert_eq!(
            disabled_reason(InstanceAction::Delete, "Survival", &running).as_deref(),
            Some("Close Minecraft first. Survival is running.")
        );
        assert_eq!(
            disabled_reason(InstanceAction::Play, "Survival", &running).as_deref(),
            Some("Already running.")
        );
        for allowed in [
            InstanceAction::OpenFolder,
            InstanceAction::Rename,
            InstanceAction::Mods,
        ] {
            assert_eq!(disabled_reason(allowed, "Survival", &running), None);
        }
        let idle = status(InstanceActivity::Idle);
        assert_eq!(
            disabled_reason(InstanceAction::Delete, "Other", &idle),
            None
        );
    }

    #[test]
    fn first_matching_reason_wins() {
        let mut busy = status(InstanceActivity::Busy(BusyOperation::Duplicating));
        busy.global.pack_task = true;
        assert_eq!(
            disabled_reason(InstanceAction::Delete, "x", &busy).as_deref(),
            Some("Wait for the copy to finish.")
        );
        let mut global = status(InstanceActivity::Idle);
        global.global.mod_task = true;
        global.global.creation_task = true;
        assert_eq!(
            disabled_reason(InstanceAction::Export, "x", &global).as_deref(),
            Some("Wait for mod changes to finish.")
        );
        assert_eq!(disabled_reason(InstanceAction::Play, "x", &global), None);
    }

    #[test]
    fn missing_folder_allows_only_remove_from_list() {
        let mut missing = status(InstanceActivity::Idle);
        missing.folder_missing = true;
        for blocked in [
            InstanceAction::Play,
            InstanceAction::Mods,
            InstanceAction::OpenFolder,
            InstanceAction::Duplicate,
            InstanceAction::Export,
            InstanceAction::ChangeVersion,
            InstanceAction::Delete,
        ] {
            assert_eq!(
                disabled_reason(blocked, "x", &missing).as_deref(),
                Some("This instance's folder is missing."),
                "{blocked:?}"
            );
        }
        assert_eq!(
            disabled_reason(InstanceAction::RemoveFromList, "x", &missing),
            None
        );
        assert_eq!(disabled_reason(InstanceAction::Rename, "x", &missing), None);
    }

    #[test]
    fn tracker_is_case_insensitive_and_exclusive() {
        let mut tracker = ActivityTracker::default();
        assert!(tracker.begin("Alpha", BusyOperation::Deleting));
        assert!(!tracker.begin("alpha", BusyOperation::Duplicating));
        assert_eq!(
            tracker.activity("ALPHA", true),
            InstanceActivity::Busy(BusyOperation::Deleting)
        );
        assert!(tracker.any(BusyOperation::Deleting));
        tracker.end("alpha");
        assert_eq!(tracker.activity("alpha", true), InstanceActivity::Running);
        assert_eq!(tracker.activity("alpha", false), InstanceActivity::Idle);
    }
}
