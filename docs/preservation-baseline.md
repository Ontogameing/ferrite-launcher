# Ferrite preservation baseline

This baseline gates the approved Option B work: retain egui and gradually extract
the reusable Rust launcher core. Stage 1 adds tests and this inventory only.
Existing behavior is the reference; known defects are not requirements to retain.

## Automated preservation inventory

| System | Existing coverage and contract to preserve |
| --- | --- |
| Instances and storage | `src/core`: stable folder identity, manifest validation, corrupt-file protection, create/edit/duplicate/remove rollback, copy cancellation, migration recovery, activity guards and staging cleanup. |
| Minecraft launching | `src/minecraft.rs`: authenticated/offline placeholders, modern and legacy arguments, paths containing spaces, explicit game directory and working directory, memory settings, tracked process lifetime. Stage 1 adds ordered OS rules, conditional argument flattening and unsupported feature omission. |
| Mod loaders | `src/loaders.rs` and backend modules: Fabric, Forge, NeoForge and Quilt remain supported. Stage 1 covers pin parsing and metadata composition, including library order, Maven paths, vanilla download/assets preservation and backend JVM differences. |
| Microsoft authentication | `src/auth.rs` and `src/app.rs`: protocol response/error handling, cancellation, sign-out, stale worker results and authenticated/offline launch gating. Accounts are session-only; client ID comes from `FERRITE_MICROSOFT_CLIENT_ID`. |
| Modrinth and instance mods | Existing API/model and mod-management tests cover dependency planning, cycles, provenance, conflicts and stale target handling. Preserve search/install/update/remove behavior and per-instance targets. |
| Imports and exports | `src/packs.rs`: Ferrite round trip, Modrinth overrides, Prism format, generic archives, CurseForge blockers, traversal/symlink rejection, size limits and atomic output replacement. Stage 1 adds app-owned cleanup of abandoned imports and retention of accepted files. |
| Configuration and appearance | `src/config.rs`, `src/ui_settings.rs`: defaults, TOML round trips, validation, legacy migration and refusal to overwrite invalid settings. Preserve existing editable UI features. |
| Background UI operations | `src/app.rs`: worker progress/results, busy guards, failure form preservation and result ownership. |
| Updates and Discord | Preserve release discovery/link behavior and optional Discord presence. Live service behavior needs the manual checks below. |

The pre-change Linux baseline passed 117 library tests and 155 binary tests
(272 total), `cargo fmt --all -- --check`, and `cargo clippy --locked --all-targets`.
Clippy already reports warnings (33 for the binary, 34 for its test target,
including duplicates); Stage 1 does not clean them up.

The final Stage 1 Linux run passed 117 library and 164 binary tests (281 total,
nine added tests), formatting, Clippy and diff whitespace checks. Clippy warning
messages match the pre-change baseline. Production Rust code and dependencies
are unchanged; all Rust additions are inside test modules.

Run the same gates after each reviewable stage:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets
cargo test --locked --all-targets --no-fail-fast
git diff --check
```

Tests use local temporary files and synthetic metadata; they do not contact
Microsoft, Mojang, Modrinth or loader services, or launch real Minecraft.

## Behavior requiring care during extraction

- Fabric currently retains vanilla JVM arguments; Quilt, Forge and NeoForge append
  loader JVM arguments. Consolidating their merges must account for this difference.
- Feature-gated Mojang arguments are currently omitted, including rules that request
  a feature value of false. OS rule matching considers the OS name only.
- Installed loader markers belong to a shared Minecraft version, rather than an
  instance. Replacing a marker can change another instance's effective loader pin.
- A successful download/install is separate from accepting instance metadata.
  Existing edit tests already cover an instance starting during installation and
  refusing the subsequent commit. Abandoned imported files require cleanup.
- Installed-version checks do not prove every library, asset or native is usable.
  Concurrent launcher writes and platform-specific filesystem/process semantics
  still need separate scrutiny.

## Stage 2 boundary fixes

Stage 2 passed 120 library and 182 binary tests on Linux (302 total, 21 added
tests), formatting, Clippy and diff whitespace checks. Clippy warning messages
and counts match Stage 1. Dependencies, persisted formats and loader merge
behavior are unchanged.

- Both accent and background color parsers reject non-ASCII input before slicing;
  valid mixed-case colors and the overlay parser's whitespace trimming survive.
- Every active launch action rechecks the existing per-instance busy/missing-folder
  guards before backend access; authentication and global busy checks retain priority.
- Native extraction rejects unsafe portable paths and archive link/special modes.
  Existing destination links and non-regular outputs are rejected before replacement;
  nested files, exclusions and ordinary reinstall repair remain supported.
- Library metadata paths are validated before cache access, downloads and classpath
  assembly, including Fabric/Quilt's separate download paths. Asset hashes require
  40 ASCII hexadecimal digits before slicing; virtual asset paths and index locations
  must stay relative. Cached legacy virtual assets retain their original filenames.
- Final process admission, spawn and registration share one lock. Polling errors
  keep the instance guard active; failed signaling/reaping retains the child handle.
  Successful stop still reaps the child and clears the slot.
- Manifest duplicate detection, activity ownership and running-instance comparisons
  reuse the existing NFC/case-normalized folder key. Alias entries are preserved as
  skipped manifest data; folder spelling and on-disk layout remain unchanged.
- Distinct migration candidates with matching manifests and file sizes are compared
  by streamed file contents. Different worlds or comparison failures require user
  choice; genuinely identical copies still auto-select.

Remaining limits: multiple distinct migration candidates can require substantial
startup disk reads; process inspection errors conservatively keep actions blocked;
and these checks do not protect against malicious concurrent replacement of parent
directories. Windows, GUI and live service/game checks below remain unexecuted.
Post-rename directory-sync failure semantics, cross-process manifest writes and
Windows staging-owner liveness remain separate fault-characterization work.

Windows native outputs follow the existing core copy policy: check links, then
open normally so file filters can operate. Microsoft documents that
`FILE_FLAG_OPEN_REPARSE_POINT` bypasses normal reparse processing; it is therefore
not forced for native repair. See [CreateFileW documentation](https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-createfilew).

## Stage 3 cleanup

The user approved deleting the four unwired customization prototypes:
`src/config/customization.rs`, `src/app/customize.rs`, `src/app/theme.rs` and
`src/app/widgets.rs` (2,932 lines). They had no Rust module wiring; the active
appearance, layout and background systems are retained.

Removed the unconstructed `LoaderNotImplemented` error, unused release-list
wrapper, unused Discord activity-update method and old Modrinth search wrapper
and parameter adapter. Existing search tests now exercise the active filter
builder directly, retaining assertions for escaping, facets and pagination.

The user explicitly retained the unused Minecraft/loader compatibility launch
wrappers and loader installation probe. All dependencies have active uses;
unread API response fields remain because deleting them can change validation.
At the Stage 3 boundary, consolidation/extraction had not begun. The 302-test Linux suite,
formatting, Clippy and whitespace gates apply; live/platform checks below remain
unexecuted.

## Stage 4 duplication consolidation

Private helpers in `src/loaders/metadata.rs` replace four identical Maven-coordinate
parsers, duplicate Fabric/Quilt profile-library conversion and download loops, and
duplicate Forge/NeoForge inherited-metadata merging, argument appending and library
normalization. Backend wrappers retain their repository defaults and diagnostic
labels. Existing ordering, empty/malformed metadata handling, cache skips, path
validation and prebuilt artifact preservation are unchanged. Fabric and Quilt
keep their separate merge functions and existing JVM policies.

Authenticated Minecraft launch wrappers share expiry checking and account-to-launch
placeholder preparation while retaining signatures, optional memory behavior and
error precedence. Appearance accent validation reuses the existing strict color
predicate in `ui_settings`; its error text is unchanged.

Five added characterization tests cover coordinate compatibility, profile conversion,
installer metadata edge cases, explicit repositories/rules and authenticated-wrapper
expiry/install-check ordering. Existing backend merge and path-safety tests remain.
The Linux suite now passes 120 library and 187 binary tests (307 total). Formatting,
Clippy and whitespace gates pass; Clippy warning messages/counts match Stage 3.

Deliberately separate: image/icon downloads and decoding have different limits,
timeouts and processing; worker handlers have distinct rollback/stale-result/dialog
policies; import installation supports pins and verifies them; RGB rendering parsers
differ in whitespace acceptance. Repeated vanilla install calls also remain because
removing one would change metadata/assets refresh and failure/progress behavior.
No dependency changes, new exported API, core extraction or Stage 5 work occurred.

## Stage 5 reusable library extraction

The existing package now exposes authentication, configuration, Minecraft, loaders,
Modrinth, local mods, packs, updates and Discord alongside the unchanged storage
`core` modules. The binary imports these implementations rather than compiling
backend copies. Unused compatibility launch wrappers and the loader probe remain
crate-private; serialized formats and dependency versions are unchanged.

Create/Edit's game-file installation now lives in `loaders::install_game_files`.
Its synchronous `InstallProgress` is adapted to the existing frontend events and
labels. Vanilla reinstallation, backend JVM differences, progress order and error
context remain unchanged.

`packs::prepare_instance_import` owns the former frontend import workflow: preview
revalidation, extraction, installation, loader pins and pin verification. Its
`PreparedImport` retains rollback ownership until dropped or consumed by `commit`.
After any returned `commit_new_instance` result, cleanup belongs to that existing
operation. In particular, a shared-folder rejection must not trigger a second
cleanup. The frontend still serializes commits and owns profiles/configuration,
account sessions, activity values, worker channels and presentation. The library
retains the process-wide single-child slot. Cancellation support is unchanged;
dropping a pack/mod worker receiver does not cancel backend writes.

All 307 existing tests remain accounted for, including the import outcome test
moved to the library. Seven new tests cover external configuration/mod access,
external launch failure precedence, installation-progress adaptation, import
commit failure, shared-folder preservation, disconnected import results and external
preview-target revalidation before writes/downloads. Default checks pass 226 library,
85 frontend and 3 integration tests (314 total). Headless checks pass the same
226 library and 3 integration tests (229 total); frontend-only tests are excluded.

Core extraction was verified before the separate final Cargo increment. `gui` is
enabled by default and required by the native binary. Disabling default features
excludes eframe, egui, image and rfd from the resolved normal dependency tree; no
workspace split or new runtime/framework was introduced. Both default and headless
builds, formatting, Clippy, tests and diff whitespace checks are required. Existing
Clippy warnings remain; library documentation still reports pre-existing storage
module warnings. Live/platform checks below remain unexecuted.

## Stage 6 application workflow ownership

The egui frontend still owns rendering, dialogs, loaded profiles/configuration and
worker scheduling. `auth::Session` owns accepted credentials, login attempts,
cancellation and result acceptance. Accounts remain session-only; offline selection
and Microsoft client-ID editing remain frontend presentation state.

`core::activity::WorkflowCoordinator` admits operations for one serialized launcher
session and releases only their originating `OperationId`. Status and launch checks
reuse existing action policy, including normalized folder ownership, authentication
error priority, missing-folder checks and the process-wide single Minecraft child.
The existing single update/removal slots and multiple distinct duplicate jobs remain.
Create can coexist with mod work; import cannot. Pending uninstall confirmation stays
frontend-owned and is supplied explicitly to admission/status checks. Lower-level
storage/network helpers remain available; callers must use the coordinator consistently
rather than mixing coordinated and uncoordinated mutations.

Create and Duplicate results now own their uncommitted folders. Failed preparation,
unwinding, disconnected channels and dropped results clean only owned files. Consuming
commits transfer cleanup after every returned manifest-commit result, including
shared-folder rejection. Duplicate cancellation semantics and retained compatibility
implementations are unchanged; cleanup ownership requires `CopiedDuplicate` to stop
being `Clone`.

`core::edit::PreparedUpdate` owns captured update inputs, blocking installation and
commit acceptance. It rechecks running state and preserves the separate version/loader
save followed by rename, including partial success if rename fails. Removal file work,
mode ordering and completion now live in `core::remove`. The separately approved fix
restores the original manifest position after permanent-removal worker-start failure.
If restoration cannot save, the profile stays in memory and the failure is explicit;
corrupt data is not overwritten. Failure after a successful permanent-delete spawn
still follows the existing manifest-before-files ordering and can leave unlisted files.

`instance_mods::workflows` owns blocking install/manage/refresh behavior, eligibility
and local-result target acceptance. Search/details rendering remains frontend-owned.
Dropping mod/import receivers still does not cancel backend writes. No service hierarchy,
event bus, universal job framework, additional runtime, dependency or serialized-format
change was introduced. The single serialized coordinator adds no cross-caller locks.

Fresh Linux verification passes formatting, whitespace checks, both Clippy modes,
both builds and both test suites. Default: 248 library + 87 frontend + 5 integration
= 340 tests. Headless: 248 library + 5 integration = 253 tests. Existing Clippy warnings
remain; none point to the new workflow implementations. An independent read-only review
of the implementation and final slot-enforcement change found no actionable regressions.

Source accounting reconstructs 314 retained Stage 5 test identities and 26 additions;
all 314 retained test basenames occur in the successful default run. Three existing
authentication test names moved from the frontend to library Session tests. The original
pre-Stage-6 snapshot was lost across session interruptions, so this is reconstructed
source accounting rather than a comparison with the original artifact. Exact full
Stage-6-only line totals cannot be established; cumulative `git diff` includes Stages
1–5. The surviving partial resume snapshot records 11 subsequently changed Rust files,
625 added and 234 removed lines; those are partial figures, not Stage 6 totals.

## Stages 7–9 follow-through

Stage 7 removes two single-use frontend loader/install adapters and repeated mod
eligibility checks. Mod worker admission now uses the existing coordinator directly;
the busy-worker test fixture records the matching owner. Guards that preserve error
priority, captured-target safety, confirmation state or UI presentation remain.
No supported behavior, dependency version, persisted format or compatibility wrapper
was removed.

Stage 8's available Linux automated checks pass: the same 340 default and 253
headless tests, both Clippy/build modes, an optimized release build and public
Rust documentation. All 340 pre-follow-through test identities are accounted for.
An independent review found no actionable cleanup regressions. Isolated Linux first
start/restart retained TOML settings; read-only Mojang and Modrinth library probes
passed. The live update endpoint returned HTTP 404. Windows, visual/interactive UI,
authentication, real games/loaders/downloads and Discord still require manual checks.
CI now runs locked commands and adds headless gates on the existing supported runners;
remote workflow jobs and release publishing were not executed.

Stage 9 adds [core API ownership documentation](core-api.md), updates README/AGENTS
for the actual session/coordinator boundary, and records [release verification](release-verification.md).
Public Rustdoc no longer links to hidden private helpers or treats a trash placeholder
as HTML. Release verification remains open until the outstanding live/platform checks
pass; documentation does not substitute for those results.

## Manual release checks

Run on Linux and Windows with disposable instances and backups of any real data.
Record OS, Java version, Minecraft/loader versions and result for each check.

1. Fresh start, restart and legacy-data migration: configuration, appearance and
   instance identity persist; corrupt settings/manifests are reported without loss.
2. Create, rename, duplicate and delete an instance containing a world. Change its
   Minecraft version/loader; force an install failure and confirm its saved selection
   and world survive. Confirm running-instance mutation guards.
3. Sign in with Microsoft, cancel a pending attempt, sign out, then sign in again.
   Launch an owned game; separately verify explicit offline mode and missing client
   ID feedback. Do not record tokens in the results.
4. Install and launch Vanilla, Fabric, Forge, NeoForge and Quilt with compatible
   Java. Include a legacy Minecraft version and a storage path containing spaces.
   Verify natives, assets, memory setting, working directory, exit and stop behavior.
5. Search Modrinth, install a mod with dependencies, update/remove it and repeat
   on a second instance. Confirm results affect the originating instance.
6. Export/reimport a disposable Ferrite instance; exercise supported Modrinth,
   Prism/generic and CurseForge imports (with required API credentials). Check
   worlds/configs, optional files, conflicts, cancellation and failed-import cleanup.
7. Check release discovery/opening the release link and Discord presence with
   Discord available and absent. Verify launcher startup and game launching still
   work when either external service is unavailable.

No Windows run, GUI smoke test, live authentication or real-game launch is claimed
by the automated baseline. Later stages must retain these checks before release.
