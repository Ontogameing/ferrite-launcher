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

**Orphaned but kept** (substantial prior work; not wired via `mod` — do not delete):

- `config/customization.rs`
- `app/customize.rs`
- `app/theme.rs`
- `app/widgets.rs`

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
