# Ferrite Launcher

Ferrite is a lightweight Minecraft launcher written in Rust (GPL-3.0).

## Why Ferrite?

Most Minecraft launchers did not fit how I play. I did not want to fork Prism — I wanted something small, toggleable, and actually customizable.

## Features

- Launch Minecraft
- Install mod loaders: Fabric, Forge, NeoForge
  - Quilt is experimental / unsupported (do not rely on it)
- Browse Modrinth (mods, modpacks, plugins, resource packs, data packs, shaders)
  - CurseForge browsing is not implemented yet
- Discord Rich Presence
- Instances: create, export, and import
  - Export: `.ferritepack`, `.mrpack`, generic / CurseForge / Prism ZIP
  - Import: same formats (still early; expect sharp edges)
- Microsoft Authentication (device-code flow; requires an approved Microsoft public client ID)
- Offline mode for local play while online auth is unavailable

## Customization status

**Active** (wired into the running UI / config):

- `ui_settings` appearance + `LayoutConfig` (stored in `ui.toml`)
- `app/layout.rs`
- Settings Appearance / Layout
- `background.rs`

The unwired customization prototypes were removed during Stage 3 cleanup.
The active appearance, layout and background systems above remain supported.

## Installation

Grab a binary from [Releases](https://github.com/Ontogameing/ferrite-launcher/releases) for Linux, Windows, or macOS.

Early development: expect bugs.

## Where Ferrite stores data

Ferrite resolves its directories once at startup from the per-user platform locations
(never from the current working directory).

**Linux** (XDG environment variables are honored):

- Config (`config.toml`, `ui.toml`): `~/.config/ferritelauncher/`
- Data (`minecraft/` with instances, versions, libraries, assets; migration state): `~/.local/share/ferritelauncher/`
- Cache (downloads, temp files): `~/.cache/ferritelauncher/`

**Windows**:

- Config: `%APPDATA%\Ferrite\Ferrite Launcher\config\` (roaming)
- Data: `%LOCALAPPDATA%\Ferrite\Ferrite Launcher\data\` (local, so game files stay out of roaming profiles)
- Cache: `%LOCALAPPDATA%\Ferrite\Ferrite Launcher\cache\`

**macOS**: config and data are the **same folder**,
`~/Library/Application Support/io.Ferrite.Ferrite-Launcher/`, which holds
`config.toml`, `ui.toml`, `minecraft/`, `migration-state.json`, and (only while a move
is in progress) `.migration-staging/`. The cache is
`~/Library/Caches/io.Ferrite.Ferrite-Launcher/`.

Older builds kept data in a `minecraft` folder relative to wherever the launcher was
started. On first launch Ferrite looks for that folder next to the executable and in
the current directory, checks there is enough free space, copies it into the data
directory (verifying the copy), and leaves the old folder untouched. If two different
old folders are found you are asked which one to move. The move can be paused (or the
window closed) and resumes on the next launch; if it can't finish, "Use old data this
time" opens Ferrite with the old folder and offers the move again next time. Ferrite
never deletes the old folder; remove it yourself once everything works.

## Building From Source

Install a recent stable Rust toolchain, then:

```bash
git clone https://github.com/Ontogameing/ferrite-launcher.git
cd ferrite-launcher
cargo run
```

Optimized release build:

```bash
cargo build --release
```

The executable is under `target/release/`.

## License

Ferrite Launcher is licensed under the [GNU General Public License v3.0 only](LICENSE).

## Contributors

- [@Ontogameing](https://github.com/Ontogameing) — Development
- [@AvatarGamingYT](https://github.com/AvatarGamingYT) — Testing & Feedback

## Contributing

Issues and pull requests are welcome. The project is early and the surface area changes often — prefer small, tested changes (especially around packs, instances, and auth).

## Rust library

The existing `ferrite_launcher` library exposes storage (`core`), authentication,
configuration, Minecraft/loaders, Modrinth/local mods, packs, updates and Discord.
Backend calls remain synchronous: run blocking operations on your own workers and
adapt progress callbacks to your frontend. Callers own loaded profiles/configuration
and worker scheduling. `auth::Session` owns account acceptance and cancellation;
`core::activity::WorkflowCoordinator` owns admission and originating-operation
completion. The library retains the single process-wide Minecraft child.

`loaders::install_game_files` is the shared Create/Edit installation sequence.
`packs::prepare_instance_import` returns a `PreparedImport`: drop it to discard the
new files or consume it with `commit` to accept the profile. Commit delegates
conflict-aware cleanup to the existing instance persistence operation, including
failed commits. Unused compatibility wrappers remain internal.

The GUI is enabled by default, so existing `cargo run` and build commands work.
To build or test the library without GUI dependencies:

```bash
cargo build --locked --no-default-features --lib
cargo test --locked --no-default-features --all-targets
```

Dependent crates can set `default-features = false` for `ferrite-launcher`.
See the [core API guide](docs/core-api.md) for ownership and workflow calls,
the [preservation baseline](docs/preservation-baseline.md) for behavior contracts,
and [release verification](docs/release-verification.md) for checks and remaining gaps.
