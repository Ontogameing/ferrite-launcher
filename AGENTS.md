# Ferrite development instructions

## Scope and approval

- Follow the user's current authorized scope. Do not treat an architecture plan
  or approval of an option as permission to implement every stage.
- The approved direction is Option B: simplify and extract a UI-independent Rust
  launcher core while retaining egui. Do not migrate to Tauri without explicit approval.
- Do not begin later refactor stages without the user's explicit approval. Complete
  verification, report the diff and risks, then stop at the authorized stage boundary.
- Stages 1 through 7 and Stage 9 documentation are complete. Stage 8 automated Linux
  verification and limited live probes are recorded; full live/platform release checks
  remain pending. Do not infer permission for Tauri, additional behavior changes or
  publishing a release. Follow `docs/release-verification.md` for outstanding checks.
- Preserve unrelated and pre-existing changes. Never reset or discard them to obtain
  a clean workspace.

## Default working approach

- Use `codex-quota-optimizer` by default when appropriate: target reads with `rg`,
  reuse prior findings, batch independent reads, and avoid redundant scans or tests.
- Use Ponytail by default for implementation and refactoring. Understand the actual
  flow first, then prefer existing repository solutions, Rust/std APIs and installed
  dependencies over new abstractions, dependencies or speculative flexibility.
- Optimize for the smallest clear, maintainable implementation, not minimum line
  count. Do not reduce verification, correctness, error handling or security to save tokens.
- Use Superpowers for structured planning and verification where applicable. Use
  Graphify when architecture/dependency investigation materially helps; validate its
  findings against Rust module wiring because archived files and tests appear in graphs.
- Use Context7 or current official documentation when library/API details require
  verification. Do not fetch documentation unnecessarily for unchanged local behavior.
- Use subagents when independent work materially improves the investigation; assign
  disjoint edit ownership and coordinate completion before final verification.

## Repository map

- Rust 2024, Cargo package `ferrite-launcher`; frontend is eframe/egui.
- `src/lib.rs` exposes the UI-independent backend modules and `core`; `src/core/`
  contains paths, instance persistence,
  filesystem operations, migration, copy/edit/duplicate/remove and activity tracking.
- `src/main.rs` wires the application modules. `src/app.rs` and `src/app/` own egui
  state, UI workflows and worker-result handling.
- Launcher systems include `minecraft`, `auth`, `loaders` (Fabric, Forge, NeoForge,
  Quilt), `modrinth`, `instance_mods`, `packs`, `config`, `ui_settings`, `updates`
  and `discord`. All except `ui_settings` are library-owned; `ui_settings`,
  `background`, `icons` and `app` remain binary/frontend-owned.
- The four unwired customization prototypes were deleted in Stage 3 with explicit
  user approval; active appearance/layout/background code remains supported.
- The user explicitly retained the unused Minecraft/loader compatibility launch
  wrappers and loader installation probe for now. Their implementations are crate-private; do not remove them without approval.
- `src/loaders/metadata.rs` contains private shared Maven/profile-library and
  installer-metadata helpers. Backend repositories and Fabric/Quilt JVM policies
  stay explicit in their existing modules.

- `loaders::install_game_files` reports synchronous `InstallProgress`; the frontend
  adapts it to existing worker events. `packs::prepare_instance_import` returns a
  `PreparedImport` guard. Its consuming commit transfers cleanup responsibility to
  `instances::commit_new_instance` after every returned result, including failure.
  Never let a second cleanup delete a folder another manifest entry claims.
- The frontend owns loaded profiles/config, presentation and worker scheduling.
  `auth::Session` owns accepted credentials and authentication attempts.
  `core::activity::WorkflowCoordinator` owns admission and owner-specific completion
  for one serialized launcher session. Retain admission until terminal result acceptance;
  release it on preparation/spawn failure or disconnect, not when cancellation is requested.
  The library retains the one process-wide Minecraft child slot.
- Create, Duplicate and Import prepared results own uncommitted folder cleanup.
  Consume their commit APIs; after every returned commit result, cleanup has transferred
  to instance persistence. Preserve folders claimed by another manifest entry.
- See `docs/core-api.md` for concrete workflows and `docs/release-verification.md`
  for automated and outstanding live/platform release checks.
- `gui` is enabled by default; the binary requires it. Headless consumers disable
  default features, excluding eframe/egui/image/rfd. Keep both build modes working.

## Preservation requirements

- Preserve instances, real Minecraft launches, authenticated and explicit offline
  modes, all four mod loaders, Modrinth/mod management, configuration and appearance,
  updates, Discord integration, supported imports/exports and existing compatibility.
- Keep stable instance folder identity, centralized absolute `AppPaths`, containment
  and archive traversal checks, corrupt-file protection, transactional persistence,
  failure rollback, cancellation and running-instance mutation guards.
- Keep worker results associated with their originating instance/task; preserve
  rejection of stale authentication results after cancellation or sign-out.
- Keep process arguments as separate arguments, including paths containing spaces;
  preserve modern/legacy metadata, explicit game directory and working directory.
- Loader merge behavior differs: Fabric retains vanilla JVM arguments; Quilt,
  Forge and NeoForge append loader JVM arguments. Preserve these differences during
  structural changes unless a separately approved behavior fix changes the contract.
- Loader installation markers are shared per Minecraft version, not per instance.
  Account state is session-only; Microsoft client ID uses
  `FERRITE_MICROSOFT_CLIENT_ID`. Do not silently change these semantics.
- Preserve Linux and Windows filesystem/process behavior. Known defects are not
  permanent requirements: document them and seek authorization for scoped fixes
  rather than hiding behavior changes inside a refactor.
- See `docs/preservation-baseline.md` for the behavior inventory, deferred risks and
  manual release checklist when that file is present.

## Verification and reporting

- Reuse existing inline `#[cfg(test)]` tests and `tempfile`; add focused regression
  or characterization coverage at important behavior boundaries without new test
  frameworks or production seams solely for easy assertions.
- Use temporary storage and synthetic metadata for offline checks. Never use real
  user instance directories or credentials in tests or logs.
- Before claiming a stage complete, run appropriate fresh checks:

  ```sh
  cargo fmt --all -- --check
  cargo clippy --locked --all-targets
  cargo test --locked --all-targets --no-fail-fast
  git diff --check
  cargo clippy --locked --no-default-features --all-targets
  cargo test --locked --no-default-features --all-targets --no-fail-fast
  ```

- The pre-Stage-1 Linux suite passed 117 library and 155 binary tests. Clippy has
  existing warnings; report new warnings separately and avoid unrelated cleanup.
- Run final checks after all edits, including subagent edits, have stopped. Do not
  claim an overlapping or earlier run verified later changes.
- Offline tests do not prove live authentication, downloads, game launches, GUI
  behavior, Discord, updates or Windows behavior. State which manual/platform checks
  were actually run and which remain unverified.
- Report exactly what changed, verification results, discovered behavior/risks and
  diff statistics. Do not proceed to the next stage automatically.
- For documentation-only changes, inspect the resulting document and its diff;
  do not rerun the Rust suite unless the change affects executable behavior.
