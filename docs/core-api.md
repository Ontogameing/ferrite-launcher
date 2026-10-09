# Using Ferrite's Rust core

The `ferrite_launcher` library contains launcher behavior without egui state.
The native binary retains egui; the `gui` feature is enabled by default. Headless
consumers use `default-features = false`. The retained compatibility launch
wrappers and loader installation probe are crate-private, not public entry points.

## Ownership and scheduling

Resolve one `core::paths::AppPaths` and use it throughout the session. It supplies
absolute storage/configuration paths; derive an instance's game directory with
`InstanceProfile::game_dir`. Its validated directory identity survives display-name
changes. Load profiles with `core::instances::load`, preserving both `profiles`
and skipped manifest entries when saving or committing changes.

The frontend owns loaded profiles/configuration, presentation, worker scheduling
and result channels. `auth::Session` owns credentials and authentication attempts.
`core::activity::WorkflowCoordinator` owns admission and operation identity for
one serialized launcher session. The library owns the process-wide Minecraft
child slot; separate coordinators do not create independent game-process slots.

Keep admission, result acceptance and manifest commits on one control thread.
Send captured inputs to workers, then return prepared results to that thread.
The coordinator adds no broad locks, cross-process protection or automatic worker
scheduling. Low-level storage/network helpers remain public: use consistent
coordination rather than mixing coordinated and uncoordinated mutations.

Network/filesystem work is blocking. Progress callbacks run synchronously on the
calling thread; forward progress to your frontend without blocking the worker.
Dropping an import/mod result receiver does not cancel backend writes. Duplicate
and migration controls provide their own cancellation; do not infer cancellation
from losing a channel.

## Admission and terminal results

Use `WorkflowCoordinator::status` and `disabled_reason` for action availability,
supplying current running state and any pending uninstall confirmation. Admit
create/import/export/mod work through `begin_create`, `begin_import`,
`begin_export` or `begin_mod`. For update, duplicate and removal, use
`begin_instance` with the matching `InstanceAction` and `BusyOperation`.
Admission rechecks coordinator ownership; stale display state is not permission.

Store the returned `OperationId` with the worker's originating target. Accept its
result only for that operation and target, then call `complete(id)` after the
terminal commit/error handling. Also release admission on preparation failure,
worker spawn failure or terminal channel disconnection. Cancellation requests
must not release ownership while a worker can still mutate files. Repeated or
stale completion cannot release a newer operation. `is_active(id)` allows stale
results to be rejected before changing session state.

The coordinator preserves a single update slot and single removal slot, while
distinct instances may be duplicated concurrently. Creation can coexist with mod
work; import cannot. Pending uninstall confirmation is frontend-owned and must
be supplied to checks. `launch` applies authentication, global work, activity and
folder checks before calling the existing process owner; explicit offline mode
remains a separate launch choice.

## Prepared workflows

| Workflow | Control thread / worker / control thread |
| --- | --- |
| Create | Allocate with `instances::new_instance_profile`; worker runs `instances::prepare_create` with an installation closure; accept `PreparedCreate::commit`. |
| Duplicate | `duplicate::prepare_duplicate`; worker runs `copy_duplicate` (or use `DuplicateTask`); accept `commit_duplicate` with the returned `CopiedDuplicate`. |
| Import | Preview with `packs::preview`, capture target/options; worker runs `packs::prepare_instance_import`; accept `PreparedImport::commit`. Preparation revalidates the preview target, installs files and verifies loader pins. |
| Update | `edit::prepare_update`; worker runs consuming `PreparedUpdate::install`; accept consuming `PreparedUpdate::commit`. |
| Remove | `remove::prepare_removal`, then `start_prepared_removal` with frontend scheduling; worker runs `run_removal_files`; accept `finish_removal`. Keep-files removal completes synchronously. |
| Export | Admit with `begin_export`; worker runs `packs::export`; accept the terminal result and release its operation. |

`PreparedCreate`, `CopiedDuplicate` and `PreparedImport` own uncommitted files.
Drop abandoned results rather than manually deleting their folders. Consuming
commit transfers cleanup responsibility to `instances::commit_new_instance`
after **every returned result**, including failure. It rechecks names and folder
claims against loaded and skipped entries. If another entry claims the folder,
it returns `FolderShared` without deleting it. A second cleanup after that error
could destroy the other entry's files.

Updates install shared version/loader files before changing instance metadata.
Preparation checks membership, loader validity and running state; commit checks
live membership and running state again. Keep coordinator admission through the
commit. Version/loader save and rename are two separate commits: an
`UpdateOutcome::rename_error` means the version/loader change succeeded but the
rename failed. Report that partial success. The instance directory never moves.
Loader installation markers are shared per Minecraft version, not per instance;
installation success does not prove a subsequent metadata commit is eligible.

Removal preserves mode-specific ordering. Permanent removal saves the manifest
before file work; a spawn failure restores the original position, reporting any
failed persistence restoration. A later file failure can leave unlisted files.
Trash commits after successful file work. Do not replace these steps with a
generic delete-and-save sequence.

## Authentication and mods

Call `auth::Session::begin_login` with the Microsoft client ID configured through
`FERRITE_MICROSOFT_CLIENT_ID`. Schedule the returned `LoginWorker::run` on a worker.
Drain `Session::poll` on the control thread and display its `SessionUpdate` values;
accepted credentials are available through `account`. `cancel` invalidates queued
and late attempt results while retaining an accepted account. `sign_out` also
forgets the account. On worker spawn failure, cancel the attempt. Credentials are
session-only: never persist or log account/token values.

For mods, use `instance_mods::workflows::can_install` before admission, supplying
live membership, project type and running state. Capture the target profile and
schedule `install` or `manage`. Both return a `LocalResult` containing the original
target, a message and refreshed local listing, including after mutation failure.
They conservatively reject mutations while any tracked Minecraft child runs.
Use `accepts_result` before updating the visible listing: the original instance
must still exist and match the currently displayed target. Switching selection
does not redirect a completed worker's files or result to the new instance.

## Verification limits

Use temporary storage and synthetic metadata for offline consumers/tests. Keep
both default and headless Cargo modes working. The preservation contracts,
automated gates and manual release checklist are in
[preservation-baseline.md](preservation-baseline.md). Offline tests do not prove
live authentication/downloads, real game launches, GUI behavior, updates, Discord
or Windows filesystem/process behavior; record those checks separately.
