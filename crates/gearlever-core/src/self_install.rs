//! Integration of Gear LeverR itself into the desktop.
//!
//! The first-run window and self-replacements write [`DESKTOP_ID`] with the
//! [`SELF_MARKER`] key. [`crate::scan`] lists that entry like any other app, and
//! the interface uses the marker to offer its dedicated self-removal flow.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::appimage::{AppImage, AppMetadata, PayloadKind, SquashfsImage, detect};
use crate::args;
use crate::desktop::DesktopFile;
use crate::error::Result;
use crate::paths::Paths;

/// Sizes tried when looking for a sharp icon, best first.
const ICON_SIZES: [&str; 5] = ["scalable", "512x512", "256x256", "128x128", "96x96"];

/// Stem of the desktop entry written for Gear LeverR.
pub const DESKTOP_STEM: &str = "io.github.alarez777.GearLeverR";

/// File name of the desktop entry written for Gear LeverR.
pub const DESKTOP_ID: &str = "io.github.alarez777.GearLeverR.desktop";

/// Icon name declared by the desktop entry, without an extension.
pub const ICON_NAME: &str = "io.github.alarez777.GearLeverR";

/// Desktop entry key that marks Gear LeverR's own integration.
pub const SELF_MARKER: &str = "X-GearLeverR-Self";

/// Desktop entry key that records which build variant an AppImage is.
pub const VARIANT_KEY: &str = "X-GearLeverR-Variant";

/// Build variant of Gear LeverR.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Variant {
    /// Uses the GTK 4 and libadwaita provided by the host.
    #[default]
    Default,
    /// Ships GTK 4 and libadwaita inside the AppImage.
    GtkIncluded,
}

impl Variant {
    /// Returns the value stored in [`VARIANT_KEY`].
    #[must_use]
    pub const fn as_key(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::GtkIncluded => "gtk-included",
        }
    }

    /// Returns the stem used for the managed AppImage file.
    #[must_use]
    pub const fn image_stem(self) -> &'static str {
        match self {
            Self::Default => "gear_leverr",
            Self::GtkIncluded => "gear_leverr_gtk_included",
        }
    }

    /// Parses a value stored in [`VARIANT_KEY`].
    fn from_key(value: &str) -> Option<Self> {
        if value.eq_ignore_ascii_case("default") {
            Some(Self::Default)
        } else if value.eq_ignore_ascii_case("gtk-included")
            || value.eq_ignore_ascii_case("gtk_included")
        {
            Some(Self::GtkIncluded)
        } else {
            None
        }
    }
}

/// Returns the build variant declared by an AppImage.
///
/// New builds record it in [`VARIANT_KEY`]. Older builds are recognised by
/// the GTK library bundled inside the SquashFS payload, and by the release file
/// name when the payload cannot be inspected.
#[must_use]
pub fn detect_variant(metadata: &AppMetadata, image: &Path) -> Variant {
    if let Some(variant) = variant_from_metadata(metadata) {
        return variant;
    }

    AppImage::open(image).map_or_else(
        |_| variant_from_file_name(image),
        |appimage| detect_variant_from_appimage(&appimage, image),
    )
}

fn detect_variant_from_appimage(appimage: &AppImage, image: &Path) -> Variant {
    match bundled_gtk(appimage) {
        Some(true) => Variant::GtkIncluded,
        Some(false) => Variant::Default,
        None => variant_from_file_name(image),
    }
}

fn variant_from_metadata(metadata: &AppMetadata) -> Option<Variant> {
    if metadata.desktop_contents.trim().is_empty() {
        return None;
    }

    let desktop = DesktopFile::parse(&metadata.desktop_contents);
    desktop
        .value("Desktop Entry", VARIANT_KEY)
        .and_then(Variant::from_key)
}

fn variant_from_file_name(image: &Path) -> Variant {
    let looks_bundled = image
        .file_name()
        .and_then(OsStr::to_str)
        .is_some_and(|name| {
            let name = name.to_ascii_lowercase();
            name.contains("gtk-included") || name.contains("gtk_included")
        });

    if looks_bundled {
        Variant::GtkIncluded
    } else {
        Variant::Default
    }
}

fn bundled_gtk(appimage: &AppImage) -> Option<bool> {
    if appimage.payload_kind() != PayloadKind::Squashfs {
        return None;
    }

    let payload = SquashfsImage::open(appimage.path(), appimage.payload_offset()).ok()?;

    Some(
        [
            "/usr/lib/libgtk-4.so.1",
            "/usr/lib/x86_64-linux-gnu/libgtk-4.so.1",
            "/lib/libgtk-4.so.1",
        ]
        .into_iter()
        .any(|path| payload.contains(Path::new(path))),
    )
}

/// Locations and policies used to install Gear LeverR itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOptions {
    /// Folder that will hold the AppImage.
    pub appimages_dir: PathBuf,
    /// Folder that will hold the desktop entry.
    pub applications_dir: PathBuf,
    /// Icon theme root, usually `$XDG_DATA_HOME/icons/hicolor`.
    pub icons_dir: PathBuf,
    /// Remove the original AppImage after a successful installation.
    pub move_source: bool,
    /// Run `update-desktop-database` when it is installed.
    pub update_desktop_database: bool,
}

impl InstallOptions {
    /// Builds the defaults for the given paths and AppImages folder.
    #[must_use]
    pub fn new(paths: &Paths, appimages_dir: impl Into<PathBuf>) -> Self {
        Self {
            appimages_dir: appimages_dir.into(),
            applications_dir: paths.applications_dir(),
            icons_dir: paths.hicolor_icons_dir(),
            move_source: true,
            update_desktop_database: true,
        }
    }
}

/// Files written by a successful installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installation {
    /// AppImage that the desktop entry runs.
    pub image_path: PathBuf,
    /// Desktop entry written for Gear LeverR.
    pub desktop_path: PathBuf,
    /// Installed icon, when the AppImage shipped one.
    pub icon_path: Option<PathBuf>,
}

/// Returns whether Gear LeverR's own desktop entry points to a usable AppImage.
///
/// An entry whose `TryExec` or `Exec` points to a missing or invalid file does
/// not count, so the first-run window can offer to repair the installation.
#[must_use]
pub fn is_installed(paths: &Paths) -> bool {
    installed_image(&paths.applications_dir().join(DESKTOP_ID)).is_some()
}

/// Returns the AppImage referenced by a desktop entry, if it still exists.
#[must_use]
pub fn installed_image(desktop_path: &Path) -> Option<PathBuf> {
    let contents = fs::read_to_string(desktop_path).ok()?;
    let desktop = DesktopFile::parse(&contents);
    let main = desktop.section("Desktop Entry")?;

    let candidate = main
        .get("TryExec")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            main.get("Exec")
                .and_then(|line| args::parse(line).ok())
                .map(|command| PathBuf::from(command.executable))
        })?;

    (candidate.is_file() && detect::image_type(&candidate).is_ok()).then_some(candidate)
}

/// Returns whether the metadata describes Gear LeverR itself.
///
/// The desktop file inside the AppImage is the reliable signal: the display
/// name can be translated or edited, while the entry name stays the same.
#[must_use]
pub fn is_self_metadata(metadata: &AppMetadata) -> bool {
    metadata
        .desktop_file
        .as_deref()
        .and_then(|file| Path::new(file).file_name())
        .is_some_and(|name| name == OsStr::new(DESKTOP_ID))
}

/// Installs the running AppImage into the application menu.
///
/// The AppImage is copied (or moved) into `options.appimages_dir`, its icon is
/// installed into the `hicolor` theme and a desktop entry is written last, so a
/// failure never leaves the menu pointing to a missing file.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] when the files cannot be written and the errors
/// documented on [`AppImage::metadata`] when the image cannot be inspected.
pub fn install(image: &Path, options: &InstallOptions) -> Result<Installation> {
    let appimage = AppImage::open(image)?;
    let metadata = appimage.metadata()?;
    let variant = variant_from_metadata(&metadata)
        .unwrap_or_else(|| detect_variant_from_appimage(&appimage, image));

    fs::create_dir_all(&options.appimages_dir)?;
    fs::create_dir_all(&options.applications_dir)?;

    let file_name = crate::install::unique_image_name(
        variant.image_stem(),
        metadata.version.as_deref(),
        &options.appimages_dir,
        &[image],
    );
    let image_path = place_image(image, &options.appimages_dir, &file_name)?;
    let icon_path = write_icon(&appimage, &metadata, &options.icons_dir)?;

    let desktop = desktop_entry(&metadata, &image_path, icon_path.as_deref(), variant);
    let desktop_path = options.applications_dir.join(DESKTOP_ID);
    write_atomic(&desktop_path, desktop.to_string().as_bytes())?;

    if options.move_source && image_path != image {
        // Everything else already succeeded, so a failure here only leaves an
        // extra copy behind. A source inside the managed folder was renamed in
        // place, so its old path is already gone.
        match fs::remove_file(image) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                tracing::warn!(path = %image.display(), %error, "cannot remove the original AppImage");
            }
        }
    }

    if options.update_desktop_database {
        crate::install::refresh_desktop_database(&options.applications_dir);
    }

    Ok(Installation {
        image_path,
        desktop_path,
        icon_path,
    })
}

/// Moves or copies the image into `directory` under `file_name`.
fn place_image(source: &Path, directory: &Path, file_name: &str) -> Result<PathBuf> {
    let source = fs::canonicalize(source)?;
    let directory = fs::canonicalize(directory)?;
    let destination = directory.join(file_name);

    if source == destination {
        return Ok(source);
    }

    // The image already lives in the managed folder: renaming it in place keeps
    // the library free of duplicate copies, even when the user chose to clone.
    if source.parent() == Some(directory.as_path()) {
        fs::rename(&source, &destination)?;
        return Ok(destination);
    }

    let temporary = temporary_path(&destination);

    let result = (|| -> Result<()> {
        fs::copy(&source, &temporary)?;
        set_executable(&temporary)?;
        fs::rename(&temporary, &destination)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;

    Ok(destination)
}

/// Installs every icon size shipped by the AppImage into the `hicolor` theme.
///
/// Installs the sharpest icon shipped by the AppImage into the `hicolor` theme.
///
/// Only one file is written so removing the entry removes every icon it owns.
/// Returns the icon for the desktop entry.
fn write_icon(
    appimage: &AppImage,
    metadata: &AppMetadata,
    icons_dir: &Path,
) -> Result<Option<PathBuf>> {
    if appimage.payload_kind() == PayloadKind::Squashfs {
        let image = SquashfsImage::open(appimage.path(), appimage.payload_offset())?;

        for size in ICON_SIZES {
            for extension in ["png", "svg"] {
                let source = PathBuf::from(format!(
                    "/usr/share/icons/hicolor/{size}/apps/{ICON_NAME}.{extension}"
                ));
                let Some(bytes) = image.read(&source)? else {
                    continue;
                };

                let destination = icons_dir
                    .join(size)
                    .join("apps")
                    .join(format!("{ICON_NAME}.{extension}"));
                write_icon_file(&destination, &bytes)?;
                update_icon_cache(icons_dir);

                return Ok(Some(destination));
            }
        }
    }

    let Some(icon) = &metadata.icon else {
        return Ok(None);
    };

    let extension = Path::new(&icon.file_name)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("png");
    let size = icon_size_directory(extension, &icon.bytes);
    let destination = icons_dir
        .join(size)
        .join("apps")
        .join(format!("{ICON_NAME}.{extension}"));

    write_icon_file(&destination, &icon.bytes)?;
    update_icon_cache(icons_dir);

    Ok(Some(destination))
}

/// Repairs the desktop entry of an existing self installation.
///
/// It disables the startup notification that leaves the cursor spinning on
/// GNOME Wayland and rewrites a themed `Icon` to the installed absolute path,
/// refreshing the icon theme cache so the shell can find the icon.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] when the desktop entry or the icon cannot be written.
pub fn refresh_entry(paths: &Paths) -> Result<()> {
    let desktop_path = paths.applications_dir().join(DESKTOP_ID);
    let Ok(contents) = fs::read_to_string(&desktop_path) else {
        return Ok(());
    };

    let mut desktop = DesktopFile::parse(&contents);
    let mut changed = false;

    if desktop
        .value("Desktop Entry", "StartupNotify")
        .is_some_and(|value| value.eq_ignore_ascii_case("true"))
    {
        desktop.set("Desktop Entry", "StartupNotify", "false");
        changed = true;
    }

    // A real absolute path already works, whether it points at the hicolor
    // theme or at the `.icons` copy written when the entry was replaced.
    let icon_is_absolute = desktop
        .value("Desktop Entry", "Icon")
        .map(Path::new)
        .is_some_and(|icon| icon.is_absolute() && icon.is_file());

    if !icon_is_absolute && let Some(icon) = best_installed_icon(&paths.hicolor_icons_dir()) {
        let icon_text = icon.to_string_lossy();
        desktop.set("Desktop Entry", "Icon", icon_text.as_ref());
        changed = true;
        update_icon_cache(&paths.hicolor_icons_dir());

        tracing::debug!(path = %icon.display(), "refreshed the Gear LeverR icon");
    }

    if !changed {
        return Ok(());
    }

    write_atomic(&desktop_path, desktop.to_string().as_bytes())
}

fn best_installed_icon(icons_dir: &Path) -> Option<PathBuf> {
    [
        "512x512", "scalable", "256x256", "128x128", "64x64", "48x48", "32x32", "24x24", "16x16",
    ]
    .into_iter()
    .flat_map(|size| {
        ["png", "svg"].map(move |extension| {
            icons_dir
                .join(size)
                .join("apps")
                .join(format!("{ICON_NAME}.{extension}"))
        })
    })
    .find(|candidate| candidate.is_file())
}

fn write_icon_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    write_atomic(path, bytes)
}

/// Returns the theme directory that matches the icon bytes.
fn icon_size_directory(extension: &str, bytes: &[u8]) -> &'static str {
    if extension.eq_ignore_ascii_case("svg") {
        return "scalable";
    }

    png_size(bytes).map_or("512x512", |size| match size {
        16 => "16x16",
        24 => "24x24",
        32 => "32x32",
        48 => "48x48",
        64 => "64x64",
        128 => "128x128",
        256 => "256x256",
        _ => "512x512",
    })
}

fn png_size(bytes: &[u8]) -> Option<u32> {
    const PNG_MAGIC: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

    if !bytes.starts_with(&PNG_MAGIC) {
        return None;
    }

    let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
    let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);

    (width == height).then_some(width)
}

fn update_icon_cache(icons_dir: &Path) {
    let status = std::process::Command::new("gtk-update-icon-cache")
        .arg("-f")
        .arg("-t")
        .arg("-q")
        .arg(icons_dir)
        .status();

    if let Err(error) = status {
        tracing::debug!(%error, "cannot refresh the icon theme cache");
    }
}

/// Builds the desktop entry, reusing the one shipped inside the AppImage.
fn desktop_entry(
    metadata: &AppMetadata,
    image: &Path,
    icon_path: Option<&Path>,
    variant: Variant,
) -> DesktopFile {
    let mut desktop = if metadata.desktop_contents.trim().is_empty() {
        DesktopFile::default()
    } else {
        DesktopFile::parse(&metadata.desktop_contents)
    };

    let image_text = image.to_string_lossy();
    let original = args::parse(&metadata.exec).unwrap_or_default();
    let exec = args::join(
        std::iter::once(image_text.as_ref()).chain(original.arguments.iter().map(String::as_str)),
    );
    // An absolute icon path lets the generic uninstall remove the file without
    // knowing that this entry belongs to Gear LeverR.
    let icon = icon_path.map_or_else(
        || ICON_NAME.to_owned(),
        |path| path.to_string_lossy().into_owned(),
    );

    if desktop
        .value("Desktop Entry", "Name")
        .is_none_or(|name| name.trim().is_empty())
    {
        desktop.set("Desktop Entry", "Name", &metadata.name);
    }

    desktop.set("Desktop Entry", "Type", "Application");
    desktop.set("Desktop Entry", "TryExec", image_text.as_ref());
    desktop.set("Desktop Entry", "Exec", exec);
    desktop.set("Desktop Entry", "Icon", icon);
    desktop.set("Desktop Entry", "Terminal", "false");
    // The startup-notification cursor can stay stuck on GNOME Wayland when the
    // pointer is over the dock, so do not request it.
    desktop.set("Desktop Entry", "StartupNotify", "false");
    desktop.set("Desktop Entry", SELF_MARKER, "true");
    desktop.set("Desktop Entry", VARIANT_KEY, variant.as_key());
    desktop.set("Desktop Entry", "X-AppImage-Name", &metadata.name);
    desktop.set(
        "Desktop Entry",
        "X-AppImage-Version",
        metadata
            .version
            .clone()
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_owned()),
    );

    desktop
}

/// Writes `contents` through a temporary file in the same directory.
fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let temporary = temporary_path(path);

    let result = (|| -> Result<()> {
        fs::write(&temporary, contents)?;
        fs::rename(&temporary, path)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }

    result
}

/// Returns a sibling path with a `.tmp` suffix.
fn temporary_path(path: &Path) -> PathBuf {
    path.file_name().map_or_else(
        || path.to_path_buf(),
        |name| path.with_file_name(format!("{}.tmp", name.to_string_lossy())),
    )
}

fn set_executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn fake_appimage(path: &Path) {
        let mut magic = vec![0_u8; 11];
        magic[0..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x00");
        magic[8..11].copy_from_slice(b"AI\x02");
        fs::write(path, magic).expect("write the fake AppImage");
        set_executable(path).expect("make it executable");
    }

    #[derive(Clone, Copy)]
    enum Payload {
        Bundled,
        Default,
    }

    fn synthetic_appimage(path: &Path, payload: Payload) {
        let header = backhand::NodeHeader {
            permissions: 0o755,
            ..Default::default()
        };
        let mut filesystem = backhand::FilesystemWriter::default();
        filesystem
            .push_dir_all("usr/lib", header)
            .expect("create the payload directories");
        if matches!(payload, Payload::Bundled) {
            filesystem
                .push_file(
                    std::io::Cursor::new(b"gtk".to_vec()),
                    "usr/lib/libgtk-4.so.1",
                    header,
                )
                .expect("add the bundled GTK library");
        }

        let mut squashfs = std::io::Cursor::new(Vec::new());
        filesystem
            .write(&mut squashfs)
            .expect("write the SquashFS payload");
        let mut bytes = vec![0_u8; 64];
        bytes[0..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2; // 64-bit ELF
        bytes[5] = 1; // little-endian
        bytes[8..11].copy_from_slice(b"AI\x02");
        bytes[0x12..0x14].copy_from_slice(&0x3e_u16.to_le_bytes());
        bytes[0x28..0x30].copy_from_slice(&64_u64.to_le_bytes());
        bytes[0x3a..0x3c].copy_from_slice(&64_u16.to_le_bytes());
        bytes.extend_from_slice(&squashfs.into_inner());
        fs::write(path, bytes).expect("write the synthetic AppImage");
    }

    fn metadata() -> AppMetadata {
        AppMetadata {
            name: "Gear LeverR".to_owned(),
            comment: "Manage AppImages with ease".to_owned(),
            exec: "gearlever-r %U".to_owned(),
            try_exec: "gearlever-r".to_owned(),
            icon_name: Some(ICON_NAME.to_owned()),
            terminal: false,
            version: Some("0.1.0".to_owned()),
            desktop_file: Some(DESKTOP_ID.to_owned()),
            desktop_contents: format!(
                "[Desktop Entry]\nName=Gear LeverR\nExec=gearlever-r %U\nIcon={ICON_NAME}\n"
            ),
            icon: None,
        }
    }

    #[test]
    fn recognizes_its_own_metadata() {
        let mut metadata = metadata();
        assert!(is_self_metadata(&metadata));

        metadata.desktop_file = Some(format!("/usr/share/applications/{DESKTOP_ID}"));
        assert!(is_self_metadata(&metadata));

        metadata.desktop_file = Some("other.desktop".to_owned());
        assert!(!is_self_metadata(&metadata));
    }

    #[test]
    fn rewrites_a_themed_icon_to_an_absolute_path() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = Paths::with_home(directory.path());
        fs::create_dir_all(paths.applications_dir()).expect("applications directory");

        let icon_directory = paths.hicolor_icons_dir().join("512x512").join("apps");
        fs::create_dir_all(&icon_directory).expect("icon directory");
        let icon = icon_directory.join(format!("{ICON_NAME}.png"));
        fs::write(&icon, b"icon").expect("icon file");

        fs::write(
            paths.applications_dir().join(DESKTOP_ID),
            format!("[Desktop Entry]\nName=Gear LeverR\nIcon={ICON_NAME}\n"),
        )
        .expect("desktop entry");

        refresh_entry(&paths).expect("refresh the entry");

        let contents =
            fs::read_to_string(paths.applications_dir().join(DESKTOP_ID)).expect("read entry");
        let desktop = DesktopFile::parse(&contents);
        assert_eq!(
            desktop.value("Desktop Entry", "Icon"),
            Some(icon.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn keeps_a_valid_absolute_icon() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = Paths::with_home(directory.path());
        fs::create_dir_all(paths.applications_dir()).expect("applications directory");

        let icon = directory.path().join("custom.png");
        fs::write(&icon, b"icon").expect("icon file");
        fs::write(
            paths.applications_dir().join(DESKTOP_ID),
            format!(
                "[Desktop Entry]\nName=Gear LeverR\nIcon={}\n",
                icon.display()
            ),
        )
        .expect("desktop entry");

        refresh_entry(&paths).expect("refresh the entry");

        let contents =
            fs::read_to_string(paths.applications_dir().join(DESKTOP_ID)).expect("read entry");
        let desktop = DesktopFile::parse(&contents);
        assert_eq!(
            desktop.value("Desktop Entry", "Icon"),
            Some(icon.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn disables_startup_notification_on_an_existing_entry() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = Paths::with_home(directory.path());
        fs::create_dir_all(paths.applications_dir()).expect("applications directory");

        let icon = directory.path().join("custom.png");
        fs::write(&icon, b"icon").expect("icon file");
        fs::write(
            paths.applications_dir().join(DESKTOP_ID),
            format!(
                "[Desktop Entry]\nName=Gear LeverR\nIcon={}\nStartupNotify=true\n",
                icon.display()
            ),
        )
        .expect("desktop entry");

        refresh_entry(&paths).expect("refresh the entry");

        let contents =
            fs::read_to_string(paths.applications_dir().join(DESKTOP_ID)).expect("read entry");
        let desktop = DesktopFile::parse(&contents);
        assert_eq!(
            desktop.value("Desktop Entry", "StartupNotify"),
            Some("false")
        );
        assert_eq!(
            desktop.value("Desktop Entry", "Icon"),
            Some(icon.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn finds_a_valid_installed_entry() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = Paths::with_home(directory.path());
        fs::create_dir_all(paths.applications_dir()).expect("applications directory");

        let image = paths.default_appimages_dir().join(ICON_NAME);
        fs::create_dir_all(paths.default_appimages_dir()).expect("appimages directory");
        fake_appimage(&image);

        fs::write(
            paths.applications_dir().join(DESKTOP_ID),
            format!(
                "[Desktop Entry]\nName=Gear LeverR\nTryExec={}\n",
                image.display()
            ),
        )
        .expect("desktop entry");

        assert!(is_installed(&paths));
    }

    #[test]
    fn stale_entries_do_not_count_as_installed() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = Paths::with_home(directory.path());
        fs::create_dir_all(paths.applications_dir()).expect("applications directory");

        fs::write(
            paths.applications_dir().join(DESKTOP_ID),
            "[Desktop Entry]\nName=Gear LeverR\nTryExec=/missing/app.AppImage\n",
        )
        .expect("desktop entry");

        assert!(!is_installed(&paths));
    }

    #[test]
    fn falls_back_to_the_exec_line() {
        let directory = tempfile::tempdir().expect("temporary home");
        let paths = Paths::with_home(directory.path());
        fs::create_dir_all(paths.applications_dir()).expect("applications directory");

        let image = paths.default_appimages_dir().join(ICON_NAME);
        fs::create_dir_all(paths.default_appimages_dir()).expect("appimages directory");
        fake_appimage(&image);

        fs::write(
            paths.applications_dir().join(DESKTOP_ID),
            format!(
                "[Desktop Entry]\nName=Gear LeverR\nExec='{}' %U\n",
                image.display()
            ),
        )
        .expect("desktop entry");

        assert_eq!(
            installed_image(&paths.applications_dir().join(DESKTOP_ID)),
            Some(image)
        );
    }

    #[test]
    fn writes_an_absolute_exec_and_the_self_marker() {
        let image = Path::new("/home/tester/AppImages/Gear LeverR.AppImage");
        let icon = Path::new("/home/tester/.local/share/icons/hicolor/512x512/apps/icon.png");
        let desktop = desktop_entry(&metadata(), image, Some(icon), Variant::Default).to_string();

        assert!(desktop.contains("TryExec=/home/tester/AppImages/Gear LeverR.AppImage"));
        assert!(desktop.contains("Exec='/home/tester/AppImages/Gear LeverR.AppImage' %U"));
        assert!(desktop.contains(&format!("{SELF_MARKER}=true")));
        assert!(desktop.contains("StartupNotify=false"));
        assert!(desktop.contains("X-AppImage-Version=0.1.0"));
        assert!(desktop.contains(&format!("{VARIANT_KEY}=default")));
        assert!(desktop.contains(&format!("Icon={}", icon.display())));
    }

    #[test]
    fn falls_back_to_the_crate_version() {
        let mut metadata = metadata();
        metadata.version = None;
        let desktop = desktop_entry(
            &metadata,
            Path::new("/home/tester/AppImages/GearLeverR.AppImage"),
            None,
            Variant::GtkIncluded,
        )
        .to_string();

        assert!(desktop.contains(&format!("X-AppImage-Version={}", env!("CARGO_PKG_VERSION"))));
        assert!(desktop.contains(&format!("{VARIANT_KEY}=gtk-included")));
        assert!(desktop.contains(&format!("Icon={ICON_NAME}")));
    }

    #[test]
    fn variant_stems_follow_the_release_names() {
        assert_eq!(Variant::Default.image_stem(), "gear_leverr");
        assert_eq!(
            Variant::GtkIncluded.image_stem(),
            "gear_leverr_gtk_included"
        );
    }

    #[test]
    fn detects_the_variant_from_the_desktop_entry() {
        let mut metadata = metadata();
        metadata.desktop_contents =
            format!("[Desktop Entry]\nName=Gear LeverR\n{VARIANT_KEY}=gtk-included\n");

        assert_eq!(
            detect_variant(&metadata, Path::new("/missing/GearLeverR.AppImage")),
            Variant::GtkIncluded
        );
    }

    #[test]
    fn falls_back_to_the_release_file_name() {
        let metadata = metadata();

        assert_eq!(
            detect_variant(
                &metadata,
                Path::new("/missing/GearLeverR-0.1.0-x86_64-gtk-included.AppImage")
            ),
            Variant::GtkIncluded
        );
        assert_eq!(
            detect_variant(
                &metadata,
                Path::new("/missing/GearLeverR-0.1.0-x86_64.AppImage")
            ),
            Variant::Default
        );
    }

    #[test]
    fn detects_the_variant_from_the_squashfs_payload() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let metadata = metadata();

        let bundled = directory.path().join("GearLeverR.AppImage");
        synthetic_appimage(&bundled, Payload::Bundled);
        assert_eq!(detect_variant(&metadata, &bundled), Variant::GtkIncluded);

        let default = directory.path().join("GearLeverR-other.AppImage");
        synthetic_appimage(&default, Payload::Default);
        assert_eq!(detect_variant(&metadata, &default), Variant::Default);
    }

    #[test]
    fn moves_a_managed_image_to_its_canonical_name() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("GearLeverR-0.1.0-x86_64.AppImage");
        fake_appimage(&source);

        let destination = place_image(&source, directory.path(), "gear_leverr.appimage")
            .expect("the image is renamed");

        assert_eq!(
            destination.file_name(),
            Some(OsStr::new("gear_leverr.appimage"))
        );
        assert!(!source.exists(), "the old name is gone");
    }

    #[test]
    fn keeps_an_image_that_already_has_the_canonical_name() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let source = directory.path().join("gear_leverr.appimage");
        fake_appimage(&source);

        let destination = place_image(&source, directory.path(), "gear_leverr.appimage")
            .expect("the image stays in place");

        assert_eq!(destination, source);
    }
}
