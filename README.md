# Gear LeverR

**English** | [Español](README.es.md)

A Rust rewrite of [Gear Lever](https://github.com/mijorus/gearlever), meant to be
faster and use less memory while keeping the same features, the same configuration
files and the same interface.

- **UI**: GTK4 + libadwaita + Relm4
- **AppImage**: two variants: the default build (uses the system GTK) and
  `gtk-included` (bundles GTK/libadwaita and their dependencies)
- **License**: GPL-3.0-or-later

## Status

Version 0.1 implements the main functional parity: integration, removal, launching,
updates from six sources, a compatible CLI and the full interface. See
[Known limitations](#known-limitations).

## Branches

- `develop`: active development. Every change lands here first.
- `main`: tested code and releases. Updated from `develop` once the tests pass.

## Features

- Integrate AppImages with one click, by drag and drop, or from the file chooser.
- Detect an AppImage that is already integrated by comparing its contents, and offer
  Launch and Remove instead of integrating a second copy.
- Ask whether to replace the installed version, keep both or cancel when a different
  AppImage with the same name is integrated.
- Integrate several AppImages at once and resolve name conflicts (keep both or replace).
- Extract the icon, name, version and description **without** running the AppImage and
  without external tools for SquashFS.
- Manage updates from a static URL, GitHub, GitLab, Codeberg, Forgejo and FTP,
  including embedded `.upd_info` data (`zsync` and `gh-releases-zsync`).
- Download only the blocks that changed when a static or GitHub source publishes
  `.zsync` data, with a full download as fallback. This delta path can be disabled in
  Preferences.
- Update everything at once, with progress, cancellation and a check for running apps.
- Edit arguments, environment variables, the website and the update source.
- Reload metadata, move an app to the trash and open its folder.
- Preferences, about dialog, search and update notifications.
- Optional GitHub token in Preferences, stored in the system keyring with an
  encrypted fallback, to raise the API rate limit from 60 to 5,000 requests per
  hour.
- First-run window that integrates Gear LeverR into the application menu and asks for
  the AppImages folder and the move/clone policy.
- gettext translations based on the original Gear Lever catalogs (22 languages).
- Update checks at login through `org.freedesktop.portal.Background`, with a classic
  `~/.config/autostart` entry as a fallback.
- CLI with a `--json` document compatible with the original.

## Requirements

- GTK 4.8 or newer and libadwaita 1.2 or newer.
- Tested on Debian 12 and Debian 13. Ubuntu 23.04+, Fedora 38+ and Arch ship
  compatible versions. **Ubuntu 22.04 does not** (GTK 4.6 / Adw 1.1).
- DwarFS or ISO9660 payloads need `dwarfsextract`/`7zz`; without them the app shows a
  clear error.

## Usage

Download one of the two AppImages, make it executable and open it:

- `GearLeverR-<version>-x86_64.AppImage`: recommended. Smaller and uses the
  system GTK 4.8 + libadwaita 1.2+.
- `GearLeverR-<version>-x86_64-gtk-included.AppImage`: GTK 4 and libadwaita are
  inside the AppImage. Use it when your distribution does not provide GTK 4.8 +
  libadwaita 1.2+.

```sh
chmod +x GearLeverR-0.1.0-x86_64.AppImage
./GearLeverR-0.1.0-x86_64.AppImage
```

The first launch shows a welcome window: it installs Gear LeverR in the application menu
(a `.desktop` entry plus the icons in `~/.local/share/icons/hicolor`) and lets you choose
the AppImages folder, whether new AppImages are moved or cloned, and whether updates are
checked at login. Once the entry exists the window is not shown again.

Gear LeverR also appears in its own list of installed AppImages. Its Remove button can
uninstall only Gear LeverR or every integrated AppImage.

You can also pass an AppImage directly:

```sh
./GearLeverR-0.1.0-x86_64.AppImage ~/Downloads/App.AppImage
```

### CLI

```sh
./GearLeverR-0.1.0-x86_64.AppImage --integrate App.AppImage
./GearLeverR-0.1.0-x86_64.AppImage --list-installed
./GearLeverR-0.1.0-x86_64.AppImage --list-installed --json
./GearLeverR-0.1.0-x86_64.AppImage --update App.AppImage
./GearLeverR-0.1.0-x86_64.AppImage --update --all
./GearLeverR-0.1.0-x86_64.AppImage --remove App.AppImage
./GearLeverR-0.1.0-x86_64.AppImage --set-update-source App.AppImage \
    --manager GithubUpdater repo=owner/repo repo_filename=App-*.AppImage allow_prereleases=false
./GearLeverR-0.1.0-x86_64.AppImage --list-update-managers
```

`--json` documents use `schema_version: 1` and the same keys as the original
(`installed` / `updates`).

## Logs

Gear LeverR writes to `~/.local/state/gearlever-r/gearlever-r.log` (or
`$XDG_STATE_HOME/gearlever-r/gearlever-r.log`).

Normal runs log warnings only. To see update details, either enable
"Enable debug logs" in Preferences → Debugging and restart, or launch from a
terminal with:

```sh
RUST_LOG=gearlever_core=info ./GearLeverR-0.1.0-x86_64.AppImage
```

Then follow the log while an update runs:

```sh
tail -f ~/.local/state/gearlever-r/gearlever-r.log
```

Delta updates are only attempted for sources that publish `.zsync` data (a
static `zsync|...` update source or a GitHub `.zsync`/`gh-releases-zsync`
asset). A successful delta update logs a summary like:

```text
INFO gearlever_core::update::delta: zsync delta downloaded 87.6 MB of 100.4 MB (12.8 MB saved, 12.8%) downloaded_bytes=91830792 saved_bytes=13451264 total_bytes=105282056 downloaded_blocks=22420 seed_blocks=3284 total_blocks=25704
```

- `downloaded_bytes` and `saved_bytes` count verified blocks; range merging can
  make the real network traffic slightly higher than `downloaded_bytes`.
- `downloaded_blocks`, `seed_blocks` and `total_blocks` are the same accounting
  in blocks.

If the delta attempt fails, Gear LeverR falls back to a full download and, with
debug logs enabled, the log shows `zsync delta failed, downloading the full
AppImage`.

## Building

Development on Debian 13 (or any distro with GTK 4.18/Adw 1.7):

```sh
sudo apt install build-essential pkg-config libgtk-4-dev libadwaita-1-dev \
    libglib2.0-dev liblzma-dev zlib1g-dev
cargo build -p gearlever --release
./target/release/gearlever-r
```

Release builds (compatible with Debian 12; glibc 2.36, GTK 4.8, Adw 1.2):

```sh
packaging/appimage/build-in-debian12.sh
```

The script builds inside `rust:1.98-bookworm` and packages both variants into
`dist/`: `...AppImage` (default, system GTK) and `...-gtk-included.AppImage`. The
`gtk-included` variant is assembled with `linuxdeploy` and its GTK plugin, which
are downloaded automatically. The resulting binaries also run on newer
distributions.

If you run the binary directly (without the AppImage), install the schema so the
preferences persist:

```sh
install -Dm644 data/io.github.alarez777.GearLeverR.gschema.xml \
    ~/.local/share/glib-2.0/schemas/
glib-compile-schemas ~/.local/share/glib-2.0/schemas
```

Logs are written to `~/.local/state/gearlever-r/gearlever-r.log` and can be opened
from the menu. Enable the `debug` level in Preferences.

## Translations

The interface uses gettext and reuses the original Gear Lever catalogs. The files live
in `po/` (`.po` format); the AppImage bundles the compiled `.mo` files.

```sh
# Compile to test locally
scripts/compile-translations.sh
GEARLEVER_R_LOCALEDIR=$PWD/target LANGUAGE=es ./target/release/gearlever-r

# After adding or changing strings with t("..."), update the template and catalogs
scripts/update-translations.sh
```

New, untranslated strings fall back to English. To complete a language, edit its `.po`
file (for example with Poedit) and compile again.

## Tests and performance

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace

# Optional tests against a real AppImage
GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test --workspace

# Benchmarks
scripts/bench-metadata.sh ~/AppImages/*.appimage
scripts/bench-startup.sh

# UI smoke test (Xvfb + openbox + xdotool): no blocking modal and real clicks work
scripts/ui-smoke.sh ./dist/GearLeverR-0.1.0-x86_64.AppImage
```

Results measured on the development machine: see
[`docs/performance.md`](docs/performance.md).

## Architecture

```
crates/gearlever-core/   GTK-free logic: appimage, desktop, config, updates, install
crates/gearlever/        binary: CLI (clap) + UI (Relm4)
data/                    .desktop, .gschema, icons
packaging/appimage/      AppRun, build.sh and build-in-debian12.sh
```

The core does not depend on GTK: the CLI runs without a display and the tests are
headless. SquashFS extraction uses `backhand` in process; the normal path does not
invoke `7zz`, `unsquashfs`, `readelf`, `file` or `ps`.

## Gear Lever compatibility

- Imports `~/.config/gearlever.conf` on first run (including the `app.<md5>` sections
  with arguments, environment variables and update sources).
- Imports the GSettings keys of the `it.mijorus.gearlever` schema.
- Writes `.desktop` files in the same format as the original.
- Uses the same `~/AppImages` and `~/AppImages/.icons` directories.
- Can coexist with the installed Flatpak.

## Known limitations

- The interface is translated by reusing the original catalogs. Spanish is complete;
  in other languages, new Gear LeverR strings may appear in English until they are
  translated (see `po/`).
- Environment variables are edited as `KEY=value` lines, not row by row.
- DwarFS and ISO9660 require `dwarfsextract` or `7zz` to be installed.
- There is no self-update for the AppImage itself.

## License and attribution

GPL-3.0-or-later. This is a derivative work of Gear Lever by Lorenzo Paderi
(<https://github.com/mijorus/gearlever>). The original name and icon are not used;
this project is distributed as Gear LeverR.

Delta updates use [zsync-rs](https://crates.io/crates/zsync-rs), MIT licensed. Its
notice is included in `LICENSES/zsync-rs-MIT.txt` and in the AppImage.
