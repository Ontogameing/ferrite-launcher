# Release verification

Automated Linux checks pass. Full Windows and live-game verification is still
pending; the checks below do not clear Ferrite for release by themselves.

## Current local results

Environment: Linux 7.2.9-1-cachyos, x86_64; Rust 1.98.1. OpenJDK 25.0.4.1 is
available, but no Java/Minecraft process was launched in these checks.

| Check | Result |
| --- | --- |
| Default tests | 248 library + 87 frontend + 5 integration = 340 passed. |
| Headless tests | 248 library + 5 integration = 253 passed. |
| Test preservation | All 340 test identities from the start of Stages 7–9 occur in the successful default run. |
| Formatting and whitespace | Passed. |
| Clippy, default and headless | Passed with existing warnings. |
| Default/headless builds | Passed. |
| Linux optimized release build | `cargo build --locked --release -j 2` passed. |
| Public Rust documentation, both modes | Passed without Rustdoc warnings after correcting private-item links and a literal HTML placeholder. |
| Headless normal dependency tree | Excludes eframe, egui, image and rfd. |
| Isolated Linux first start and restart | Both processes stayed alive for 12 seconds; UI settings initialized and TOML settings were unchanged on restart. |
| Mojang live release catalog | Current library decoded 103 release entries. |
| Modrinth live catalog | Current library decoded search, project details, versions and team responses. |
| Live update discovery | Failed: the configured latest-release endpoint returned HTTP 404. |

The Linux startup probes used temporary XDG config/data/cache roots, a copied
executable and a temporary working directory. They did not scan or mutate real
user instances. Discord and automatic update checks were disabled in the probe
configuration; no Microsoft client ID or credentials were supplied. Processes
were terminated after the bounded smoke interval. This establishes initialization
and settings retention, not visual correctness, interaction or graceful shutdown.
Public service probes made read-only metadata requests; they did not install a
game, loader or mod.

The update endpoint remains:
`https://api.github.com/repos/Ontogameing/ferrite-launcher/releases/latest`.
A 404 does not distinguish an absent published release from an unavailable/private
repository. Confirm repository access and a compatible published release before
claiming live update discovery works. No repository URL, authentication or updater
error behavior was changed to hide this failure.

## Repeatable automated gates

Run from the repository root after source edits stop:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets
cargo test --locked --all-targets --no-fail-fast
cargo clippy --locked --no-default-features --all-targets
cargo test --locked --no-default-features --all-targets --no-fail-fast
cargo build --locked
cargo build --locked --no-default-features --lib
cargo build --locked --release
cargo doc --locked --no-deps
cargo doc --locked --no-default-features --no-deps --lib
git diff --check
```

The existing GitHub Actions workflow now checks locked dependencies, runs Linux
headless Clippy/tests/build, and adds headless tests to the Windows/macOS release
build jobs. Workflow YAML was parsed and inspected locally. No remote jobs were
triggered during this work; configuration is not evidence of platform success.
Tagged-release publishing remains the existing workflow and was not invoked.

## Outstanding release checks

Run the complete [preservation checklist](preservation-baseline.md#manual-release-checks)
on Linux and Windows using disposable instances. Record OS, Java, Minecraft and
loader versions, observed results and failures. Check:

- Fresh start/restart, legacy migration, corruption handling and appearance/layout.
- Create, rename, update, duplicate, remove/trash/delete, rollback and running guards.
- Microsoft sign-in with an approved public client ID, cancellation, sign-out,
  late-result rejection, authenticated launch and explicit offline launch.
- Real Vanilla, Fabric, Forge, NeoForge and Quilt installation/launch, including a
  legacy Minecraft version and a storage path containing spaces. Verify compatible
  Java, natives/assets, memory, working directory, stop/reaping and process exclusion.
- Mod installation, dependencies, enable/disable/uninstall and originating-target
  acceptance across two instances.
- Supported pack formats, worlds/configuration, overrides/pins, cancellation and
  failed-import cleanup; CurseForge credentials where required.
- Successful update discovery/link opening and optional Discord behavior with the
  service present and absent.

No Windows runtime, visual/interactive GUI check, live Microsoft authentication,
real-game launch, live download/install, Discord integration or full import/export
release scenario is claimed by the current local results. Never put access tokens
or account credentials in verification records. Permanent-delete failures after a
successful worker start can still leave unlisted files; preserve and report that
known behavior until a separate correctness fix is approved.
