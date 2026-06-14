# APR — Arch Package Remover

An interactive terminal UI for safely removing packages on Arch Linux (and
other pacman/ALPM-based distros), built with [ratatui](https://ratatui.rs).

[Русская версия README](README.ru.md)

![license](https://img.shields.io/badge/license-MIT-blue)
![CI](https://github.com/TeddyBear/arch-pkg-remover/actions/workflows/ci.yml/badge.svg)
![Release](https://github.com/TeddyBear/arch-pkg-remover/actions/workflows/release.yml/badge.svg)

## Features

- Browse, search and filter your installed packages
- Mark multiple packages for removal
- **Real dry-run**: resolves the actual ALPM transaction (including
  transitive dependencies) before anything is removed
- Shows "Required By" (reverse dependencies) for each package
- Orphan detection — quickly find packages installed as dependencies that
  nothing else needs anymore
- One-key **"mark all orphans"** action for fast cleanup
- Built-in help screen listing every keybinding (`?`)
- Sort by name, size, or orphans-first
- Multiple removal modes: simple (`-R`), recursive (`-Rs`), full (`-Rns`),
  and force (`-Rdd`)
- Warns before removing packages from a built-in list of critical system
  packages
- Live progress and log view during removal
- Refresh the package list without restarting

## Installation

### Prebuilt binary

Every tagged release (`vX.Y.Z`) is built automatically by GitHub Actions and
attached to the [Releases](https://github.com/TeddyBear/arch-pkg-remover/releases)
page as `apr-vX.Y.Z-x86_64-linux.tar.gz`. Download it, extract, and run:

```sh
tar -xzf apr-vX.Y.Z-x86_64-linux.tar.gz
sudo ./apr
```

### Build from source

Requires Rust (stable) and the `pacman`/`alpm` development headers, which are
already present on any standard Arch installation.

```sh
git clone https://github.com/TeddyBear/arch-pkg-remover.git
cd arch-pkg-remover
cargo build --release
```

The resulting binary will be at `target/release/apr`.

## Usage

APR modifies the package database, so it needs root privileges:

```sh
sudo ./target/release/apr
```

### Keybindings

| Key            | Action                                  |
|----------------|------------------------------------------|
| `↑`/`↓`, `j`/`k` | Move selection                          |
| `g` / `G`      | Jump to top / bottom                    |
| `Space`        | Toggle mark on the selected package     |
| `a`            | Mark/unmark all visible packages        |
| `O`            | Mark all orphans (quick cleanup)        |
| `/`            | Search (filters by name, description, version) |
| `Esc`          | Clear search / orphan filter            |
| `o`            | Toggle "orphans only" filter            |
| `s`            | Cycle sort mode (Name / Size / Orphans-first) |
| `1`–`4`, `Tab` | Set / cycle removal mode (`-R`, `-Rs`, `-Rns`, `-Rdd`) |
| `d`            | Run dry-run and open the confirmation dialog |
| `r`            | Refresh the package list                |
| `?`            | Show the help screen                    |
| `q`            | Quit                                    |
| `Ctrl+C`       | Force quit                              |

In the confirmation dialog, type `yes` and press `Enter` to proceed with
removal, or `Esc` to cancel.

## Removal modes

| Mode | Flag    | Description                              |
|------|---------|-------------------------------------------|
| Simple    | `-R`   | Remove only the selected packages          |
| Recursive | `-Rs`  | Also remove now-unused dependencies         |
| Full      | `-Rns` | Recursive + remove configuration files      |
| Force     | `-Rdd` | Ignore dependency checks (use with caution!) |

## Safety notes

- A dry-run always runs before the actual removal, showing the full list of
  packages that will be affected (including transitive dependencies pulled
  in by recursive modes) and the total disk space that will be freed.
- If any package on the removal list is considered part of the core system
  (e.g. `glibc`, `systemd`, the `linux` kernel package, `pacman` itself), APR
  shows a warning in the confirmation dialog.
- This tool directly manipulates the pacman database via libalpm. **Always
  read the confirmation screen carefully before typing `yes`.**

## CI/CD

This repo uses GitHub Actions (workflows in `.github/workflows/`):

- **`ci.yml`** — runs on every push to `main` and on pull requests. Builds
  the project in an Arch Linux container (debug + release), and runs
  `cargo fmt --check` / `cargo clippy`.
- **`release.yml`** — triggered by pushing a tag matching `v*` (e.g.
  `v0.2.0`). Builds a release binary, strips it, packages it as
  `apr-vX.Y.Z-x86_64-linux.tar.gz`, and publishes it on the
  [Releases](https://github.com/TeddyBear/arch-pkg-remover/releases) page
  with auto-generated release notes.

To cut a new release:

```sh
git tag v0.2.1
git push origin v0.2.1
```

## License

Licensed under the [MIT License](LICENSE).
