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

- `config` appearance + `LayoutConfig`
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
