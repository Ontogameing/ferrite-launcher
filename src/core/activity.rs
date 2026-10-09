//! Per-instance activity and the reasons an action is unavailable.
//!
//! [`WorkflowCoordinator`] admits work and derives card status for the serialized
//! launcher session. Frontends schedule workers and complete the originating operation
//! after accepting its result. Existing action rules keep their first-match priority.

use crate::core::manifest::directory_key;
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
        let key = directory_key(directory);
        if self.busy.contains_key(&key) {
            return false;
        }
        self.busy.insert(key, operation);
        true
    }

    /// Clears whatever operation held `directory`.
    pub fn end(&mut self, directory: &str) {
        self.busy.remove(&directory_key(directory));
    }

    pub fn busy(&self, directory: &str) -> Option<BusyOperation> {
        self.busy.get(&directory_key(directory)).copied()
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

/// Identifies one admitted operation. Only its matching completion can release it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct OperationId(u64);

#[derive(Debug)]
enum Workflow {
    Instance(String),
    Create,
    Pack,
    Mod,
}

/// UI-independent admission and ownership for existing launcher workflows.
/// The frontend serializes calls and continues to own workers and their results.
#[derive(Debug, Default)]
pub struct WorkflowCoordinator {
    activity: ActivityTracker,
    operations: HashMap<OperationId, Workflow>,
    next_id: u64,
}

impl WorkflowCoordinator {
    /// Authentication keeps its existing priority over selection and activity errors.
    pub fn launch_auth_error(
        account: Option<&crate::auth::Account>,
        offline: bool,
    ) -> Option<&'static str> {
        if offline {
            return None;
        }
        match account {
            None => Some("Sign in with Microsoft before launching, or select Offline mode."),
            Some(account) if account.is_expired() => {
                Some("Session expired. Sign in again before launching.")
            }
            Some(_) => None,
        }
    }

    /// Launches through the existing single-child process owner after workflow admission.
    #[allow(clippy::too_many_arguments)]
    pub fn launch(
        &self,
        paths: &crate::core::paths::AppPaths,
        profile: Option<&crate::core::instances::InstanceProfile>,
        account: Option<&crate::auth::Account>,
        offline: bool,
        memory_mb: u32,
        pending_uninstall: bool,
    ) -> Result<String, String> {
        if let Some(reason) = Self::launch_auth_error(account, offline) {
            return Err(reason.into());
        }
        let global = self.global_busy(pending_uninstall);
        if global.mod_task || global.pack_task {
            return Err(
                "Wait for mod or instance import/export work to finish before launching.".into(),
            );
        }
        let profile = profile.ok_or_else(|| "Select an instance before launching.".to_owned())?;
        let status = self.status(
            paths,
            profile,
            crate::minecraft::is_instance_running(profile.directory().as_str()),
            pending_uninstall,
        );
        if let Some(reason) = disabled_reason(InstanceAction::Play, &profile.name, &status) {
            return Err(reason);
        }
        let loader = crate::loaders::ModLoader::from_label(&profile.loader)
            .ok_or_else(|| format!("Unknown mod loader: {}", profile.loader))?;
        let game_dir = profile.game_dir(paths);
        let result = if offline {
            crate::loaders::launch_in_directory_with_memory(
                paths,
                &profile.version,
                loader,
                &game_dir,
                memory_mb,
            )
        } else {
            crate::loaders::launch_authenticated_with_memory(
                paths,
                &profile.version,
                loader,
                &game_dir,
                account.expect("checked above"),
                memory_mb,
            )
        };
        result.map_err(|error| format!("Failed to launch '{}': {error}", profile.name))?;
        Ok(if offline {
            format!("Launched '{}' in offline mode.", profile.name)
        } else {
            format!("Launched '{}'.", profile.name)
        })
    }

    fn admit(&mut self, workflow: Workflow) -> OperationId {
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("operation IDs exhausted");
        let id = OperationId(self.next_id);
        self.operations.insert(id, workflow);
        id
    }

    pub fn global_busy(&self, pending_uninstall: bool) -> GlobalBusy {
        GlobalBusy {
            pack_task: self
                .operations
                .values()
                .any(|job| matches!(job, Workflow::Pack)),
            mod_task: pending_uninstall
                || self
                    .operations
                    .values()
                    .any(|job| matches!(job, Workflow::Mod)),
            creation_task: self
                .operations
                .values()
                .any(|job| matches!(job, Workflow::Create)),
        }
    }

    pub fn status(
        &self,
        paths: &crate::core::paths::AppPaths,
        profile: &crate::core::manifest::InstanceProfile,
        running: bool,
        pending_uninstall: bool,
    ) -> InstanceStatus {
        InstanceStatus {
            activity: self
                .activity
                .activity(profile.directory().as_str(), running),
            folder_missing: crate::core::instances::folder_missing(paths, profile),
            global: self.global_busy(pending_uninstall),
        }
    }

    pub fn create_import_lock(&self) -> Option<String> {
        if self.global_busy(false).pack_task {
            Some("Wait for the import or export to finish.".into())
        } else if self.any(BusyOperation::Updating) {
            Some("Wait for Ferrite to finish updating the instance.".into())
        } else {
            None
        }
    }

    pub fn begin_create(&mut self) -> Result<OperationId, String> {
        if let Some(reason) = self.create_import_lock() {
            return Err(reason);
        }
        if self.global_busy(false).creation_task {
            return Err("Wait for the new instance to finish installing.".into());
        }
        Ok(self.admit(Workflow::Create))
    }

    pub fn begin_import(&mut self, pending_uninstall: bool) -> Result<OperationId, String> {
        if let Some(reason) = self.create_import_lock() {
            return Err(reason);
        }
        let global = self.global_busy(pending_uninstall);
        if global.creation_task {
            return Err("Wait for the new instance to finish installing.".into());
        }
        if global.mod_task {
            return Err("Wait for mod changes to finish.".into());
        }
        Ok(self.admit(Workflow::Pack))
    }

    pub fn begin_export(
        &mut self,
        profile: &crate::core::manifest::InstanceProfile,
        status: &InstanceStatus,
    ) -> Result<OperationId, String> {
        self.check_instance(InstanceAction::Export, profile, status)?;
        Ok(self.admit(Workflow::Pack))
    }

    pub fn begin_mod(&mut self, pending_uninstall: bool) -> Result<OperationId, String> {
        let global = self.global_busy(pending_uninstall);
        if global.mod_task {
            return Err("Wait for mod changes to finish.".into());
        }
        if global.pack_task {
            return Err("Wait for the import or export to finish.".into());
        }
        Ok(self.admit(Workflow::Mod))
    }

    fn check_instance(
        &self,
        action: InstanceAction,
        profile: &crate::core::manifest::InstanceProfile,
        status: &InstanceStatus,
    ) -> Result<(), String> {
        let status = InstanceStatus {
            activity: self.activity.activity(
                profile.directory().as_str(),
                status.activity == InstanceActivity::Running,
            ),
            global: self.global_busy(status.global.mod_task),
            folder_missing: status.folder_missing,
        };
        disabled_reason(action, &profile.name, &status).map_or(Ok(()), Err)
    }

    pub fn begin_instance(
        &mut self,
        action: InstanceAction,
        profile: &crate::core::manifest::InstanceProfile,
        status: &InstanceStatus,
        operation: BusyOperation,
    ) -> Result<OperationId, String> {
        if !matches!(
            (action, operation),
            (InstanceAction::ChangeVersion, BusyOperation::Updating)
                | (InstanceAction::Duplicate, BusyOperation::Duplicating)
                | (
                    InstanceAction::Delete | InstanceAction::RemoveFromList,
                    BusyOperation::Deleting
                )
        ) {
            return Err("The instance action does not match its operation.".into());
        }
        if operation == BusyOperation::Updating && self.any(operation) {
            return Err("Ferrite is already updating an instance.".into());
        }
        if operation == BusyOperation::Deleting && self.any(operation) {
            return Err("Ferrite is already removing an instance.".into());
        }
        self.check_instance(action, profile, status)?;
        let directory = profile.directory().as_str();
        if !self.activity.begin(directory, operation) {
            return Err("Wait for this instance's operation to finish.".into());
        }
        Ok(self.admit(Workflow::Instance(directory.to_owned())))
    }

    /// Releases exactly this operation; stale or repeated completions do nothing.
    pub fn complete(&mut self, id: OperationId) -> bool {
        let Some(workflow) = self.operations.remove(&id) else {
            return false;
        };
        if let Workflow::Instance(directory) = workflow {
            self.activity.end(&directory);
        }
        true
    }

    pub fn is_active(&self, id: OperationId) -> bool {
        self.operations.contains_key(&id)
    }

    pub fn any(&self, operation: BusyOperation) -> bool {
        self.activity.any(operation)
    }

    pub fn busy(&self, directory: &str) -> Option<BusyOperation> {
        self.activity.busy(directory)
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

    fn profile(name: &str) -> crate::core::manifest::InstanceProfile {
        crate::core::manifest::InstanceProfile::new(
            name.into(),
            "1.21".into(),
            "Vanilla".into(),
            &[],
        )
    }

    #[test]
    fn coordinator_preserves_single_update_and_removal_slots() {
        for (action, operation) in [
            (InstanceAction::ChangeVersion, BusyOperation::Updating),
            (InstanceAction::Delete, BusyOperation::Deleting),
        ] {
            let mut coordinator = WorkflowCoordinator::default();
            let first = profile("First");
            let second = profile("Second");
            let idle = status(InstanceActivity::Idle);
            let owner = coordinator
                .begin_instance(action, &first, &idle, operation)
                .unwrap();
            assert!(
                coordinator
                    .begin_instance(action, &second, &idle, operation)
                    .is_err()
            );
            assert!(coordinator.is_active(owner));
            coordinator.complete(owner);
            assert!(
                coordinator
                    .begin_instance(action, &second, &idle, operation)
                    .is_ok()
            );
        }
        let mut coordinator = WorkflowCoordinator::default();
        let idle = status(InstanceActivity::Idle);
        assert!(
            coordinator
                .begin_instance(
                    InstanceAction::Duplicate,
                    &profile("First"),
                    &idle,
                    BusyOperation::Duplicating
                )
                .is_ok()
        );
        assert!(
            coordinator
                .begin_instance(
                    InstanceAction::Duplicate,
                    &profile("Second"),
                    &idle,
                    BusyOperation::Duplicating
                )
                .is_ok()
        );
    }

    #[test]
    fn coordinator_rejects_stale_completion_and_folder_aliases() {
        let mut coordinator = WorkflowCoordinator::default();
        let with_directory = |directory: &str| {
            serde_json::from_value::<crate::core::manifest::InstanceProfile>(serde_json::json!({
                "name": "Example", "version": "1.21", "loader": "Vanilla", "directory": directory
            }))
            .unwrap()
        };
        let original = with_directory("Café");
        let alias = with_directory("cafe\u{301}");
        let idle = status(InstanceActivity::Idle);
        let old = coordinator
            .begin_instance(
                InstanceAction::Duplicate,
                &original,
                &idle,
                BusyOperation::Duplicating,
            )
            .unwrap();
        assert!(
            coordinator
                .begin_instance(
                    InstanceAction::Delete,
                    &alias,
                    &idle,
                    BusyOperation::Deleting
                )
                .is_err()
        );
        assert!(coordinator.complete(old));
        let new = coordinator
            .begin_instance(
                InstanceAction::Delete,
                &alias,
                &idle,
                BusyOperation::Deleting,
            )
            .unwrap();
        assert!(!coordinator.complete(old));
        assert!(coordinator.is_active(new));
        assert_eq!(
            coordinator.busy(original.directory().as_str()),
            Some(BusyOperation::Deleting)
        );
        assert!(coordinator.complete(new));
        assert!(!coordinator.any(BusyOperation::Deleting));
    }

    #[test]
    fn coordinator_preserves_creation_mod_import_asymmetry() {
        let mut coordinator = WorkflowCoordinator::default();
        let mods = coordinator.begin_mod(false).unwrap();
        let create = coordinator.begin_create().unwrap();
        assert!(coordinator.begin_import(false).is_err());
        assert!(coordinator.begin_create().is_err());
        assert!(coordinator.complete(create));
        assert!(coordinator.begin_import(false).is_err());
        assert!(coordinator.complete(mods));
        assert!(coordinator.begin_mod(true).is_err());
        assert!(coordinator.begin_import(true).is_err());
        let import = coordinator.begin_import(false).unwrap();
        assert!(coordinator.begin_create().is_err());
        assert!(coordinator.begin_mod(false).is_err());
        assert!(coordinator.complete(import));
        let replacement = coordinator.begin_import(false).unwrap();
        assert!(!coordinator.complete(import));
        assert!(coordinator.global_busy(false).pack_task);
        assert!(coordinator.complete(replacement));
    }

    #[test]
    fn coordinator_rechecks_owned_locks_and_preserves_other_instance_running() {
        let mut coordinator = WorkflowCoordinator::default();
        let first = profile("First");
        let second = profile("Second");
        let idle = status(InstanceActivity::Idle);
        assert!(
            coordinator
                .begin_export(&first, &status(InstanceActivity::Running))
                .is_err()
        );
        let update = coordinator
            .begin_instance(
                InstanceAction::ChangeVersion,
                &first,
                &idle,
                BusyOperation::Updating,
            )
            .unwrap();
        assert!(coordinator.create_import_lock().is_some());
        assert!(coordinator.begin_import(false).is_err());
        assert!(coordinator.begin_create().is_err());
        assert!(coordinator.begin_export(&first, &idle).is_err());
        let export = coordinator.begin_export(&second, &idle).unwrap();
        assert!(
            coordinator
                .begin_instance(
                    InstanceAction::Delete,
                    &second,
                    &idle,
                    BusyOperation::Deleting
                )
                .is_err()
        );
        assert!(coordinator.complete(export));
        assert!(coordinator.complete(update));
        let mut missing = idle;
        missing.folder_missing = true;
        assert!(coordinator.begin_export(&first, &missing).is_err());
        assert!(
            coordinator
                .begin_instance(
                    InstanceAction::RemoveFromList,
                    &first,
                    &missing,
                    BusyOperation::Deleting
                )
                .is_ok()
        );
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

    #[test]
    fn tracker_owns_unicode_folder_aliases_exclusively() {
        for (first, alias) in [("Ä", "ä"), ("Café", "cafe\u{301}")] {
            let mut tracker = ActivityTracker::default();
            assert!(tracker.begin(first, BusyOperation::Duplicating));
            assert!(
                !tracker.begin(alias, BusyOperation::Deleting),
                "{first:?} and {alias:?} must share ownership"
            );
            assert_eq!(tracker.busy(alias), Some(BusyOperation::Duplicating));
            assert_eq!(
                tracker.activity(alias, false),
                InstanceActivity::Busy(BusyOperation::Duplicating)
            );
            tracker.end(alias);
            assert_eq!(tracker.busy(first), None);
            assert_eq!(tracker.activity(first, false), InstanceActivity::Idle);
        }
    }
}
