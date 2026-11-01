# AGENTS.md — Gear LeverR

## Project identity

Rust rewrite of [Gear Lever](https://github.com/mijorus/gearlever) (GPL-3.0-or-later).
The original is cloned at `/home/alarez/Proyectos/gearlever`; it is a **behaviour
reference only** and must not be modified. That directory has its own `AGENTS.md`.

| Field | Value |
| --- | --- |
| Name | Gear LeverR |
| App ID | `io.github.alarez777.GearLeverR` |
| Binary | `gearlever-r` |
| AppImage | `GearLeverR-<version>-x86_64.AppImage` |
| Config | `~/.config/gearlever-r.conf` |
| License | GPL-3.0-or-later (derivative work; keep attribution to Lorenzo Paderi) |

## Required Rust skills (BeautifulRust)

Installed in `~/.config/opencode/skills/`. **Always use them**, without waiting to
be asked:

| Skill | When |
| --- | --- |
| `rust-best-practices` | Before writing or refactoring any Rust code. |
| `rust-review` | When reviewing code, a `.rs` file, a crate, a diff or a PR. |
| `rust-port` | When porting code from C, C++, Python, Go, JS or TS. |

Style and API rules come from the skill; do not improvise them.

## Stack

- Rust edition 2024, `rust-version = "1.93"` (required by Relm4 0.11).
- UI: **GTK4 + libadwaita + Relm4 0.11** (`features = ["libadwaita"]`).
  - It is compiled against GTK 4.18 / Adw 1.7 (Debian 13) but **the API used must
    exist in GTK 4.8 / libadwaita 1.2** so Debian 12 and Ubuntu 23.04+ keep working.
  - Not allowed for now: `AdwToolbarView`, `AdwNavigationView`, `AdwBanner`,
    `AdwToggleGroup`, `AdwAlertDialog` (they are 1.3/1.4/1.5). Use local equivalents.
- Background work: std threads reporting through Relm4 messages; no async runtime
  in project code. Do not touch GTK from a worker thread.
- HTTP: `ureq` with **rustls** (never OpenSSL, so the AppImage does not depend on it).
- CLI: `clap` (derive). Errors: `thiserror` in the library, `anyhow` only in the binary.
- SquashFS: `backhand` (no external binaries). DwarFS and ISO9660 use the external
  `dwarfsextract`/`7zz` fallback, gated by the safe-extractor setting.
- FTP: `suppaftp`. Hashing: `md-5`, `sha1`, `sha2`, `blake3`.
- No `unwrap()`/`expect()` on production paths; `expect("reason")` only for invariants.

**Release builds:** the AppImage is built inside a **Debian 12** container (glibc 2.36,
GTK 4.8, Adw 1.2) so it keeps running on Debian 12 even though development happens on
Debian 13. Debian 13 binaries are for testing only. Use
`packaging/appimage/build-in-debian12.sh`; do not build the AppImage on the host.

## Layout

```
crates/gearlever-core/   GTK-free logic: appimage, desktop, config, updates, install
crates/gearlever/        binary: CLI (clap) + UI (Relm4)
  src/ui/                window, details, preferences, first-run setup, autostart
  tests/                 CLI integration tests
crates/gearlever-core/tests/  core and end-to-end tests
data/                    .desktop, GSettings schema, icons (16..512 px)
po/                      upstream catalogs, overrides and compiled translations
packaging/appimage/      AppRun, build scripts and CI recipe
scripts/                 benchmarks, translation tooling, UI smoke test
docs/                    performance notes
```

The core **does not** depend on GTK: the CLI runs without a display and the tests are
headless.

## Gear Lever compatibility (required)

- Read `~/.config/gearlever.conf` (sections `app.<md5(path)>` and
  `app.<md5>.update_manager`) when importing; write `gearlever-r.conf` from then on.
- Write `.desktop` files with the same format: `TryExec`, `Exec`, `Icon`, `Actions`,
  `X-AppImage-Version`, `X-AppImage-Name`, `DESKTOPINTEGRATION=1`.
- Keep the same update manager IDs: `StaticFileUpdater`, `GithubUpdater`,
  `GitlabUpdater`, `CodebergUpdater`, `FTPUpdater`, `ForgejoUpdater`.
- Keep CLI behaviour and `--json` with `schema_version: 1`.
- Gear LeverR installs its own `.desktop` entry with `X-GearLeverR-Self=true`; `scan()`
  lists it like any other app and the UI uses the marker to offer self-removal.
- Do not change the AppImages folder or `.icons` (`~/AppImages` by default).

## Quality before handing back

```sh
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

- Lints live in `[workspace.lints]` (from BeautifulRust) and `clippy.toml` at the root.
- Suppress with `#[expect(lint, reason = "...")]`, never `#[allow]` without a reason.
- Tests live next to the code or under `tests/`; anything touching network or time
  must be mockable.
- Before packaging, compile translations with `scripts/compile-translations.sh`.

## Git workflow

- Every change starts in its own branch created from `develop` (`fix/...`,
  `feat/...`, `chore/...`). Never commit directly to `develop` or `main`.
- Do not merge until it is tested: the quality commands above and, for UI or
  packaging changes, `scripts/ui-smoke.sh` in the Debian 12 container.
- Merge the branch into `develop`, then into `main`, and push both.
- Delete the branch (local and remote) once it is merged: a branch reaching
  `develop` means it is tested and its purpose is fulfilled.

## Conventions

- Speak Spanish with the user; use English for identifiers, docs and commit messages.
- Every user-visible string goes through `t("...")` (or `tf` with `{placeholders}`)
  and reuses the original Gear Lever msgid when it exists. After changing strings,
  run `scripts/update-translations.sh` and `scripts/compile-translations.sh`.
- Keep commits small and reviewable; no unrequested mass refactors.
- Do not add new features: the goal is parity plus performance.
- Measure before optimizing; benchmarks live in `benches/` or `scripts/`.
- After UI changes, run `scripts/ui-smoke.sh` in a Debian 12 container with
  Xvfb + openbox + xdotool. It clicks with a real pointer, which is what catches
  modal windows and overlays that block the main window.
