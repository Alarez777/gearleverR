//! Integration of Gear LeverR itself into the desktop.
//!
//! Gear LeverR manages AppImages, but it does not manage its own desktop entry:
//! the first-run window writes it once and [`crate::scan`] skips it thanks to the
//! [`SELF_MARKER`] key, so the application cannot uninstall itself by accident.

use std::ffi::OsStr;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::appimage::{AppImage, AppMetadata, detect};
use crate::args;
use crate::desktop::DesktopFile;
use crate::error::{Error, Result};
use crate::paths::Paths;

/// File name of the desktop entry written for Gear LeverR.
pub const DESKTOP_ID: &str = "io.github.alarez777.GearLeverR.desktop";

/// Icon name declared by the desktop entry, without an extension.
pub const ICON_NAME: &str = "io.github.alarez777.GearLeverR";

/// Desktop entry key that keeps Gear LeverR out of its own list.
pub const SELF_MARKER: &str = "X-GearLeverR-Self";

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

/// Installs the running AppImage into the application menu.
///
/// The AppImage is copied (or moved) into `options.appimages_dir`, its icon is
/// installed into the `hicolor` theme and a desktop entry is written last, so a
/// failure never leaves the menu pointing to a missing file.
///
/// # Errors
///
/// Returns [`Error::Io`] when the files cannot be written and the errors
/// documented on [`AppImage::metadata`] when the image cannot be inspected.
pub fn install(image: &Path, options: &InstallOptions) -> Result<Installation> {
    let metadata = AppImage::open(image)?.metadata()?;

    fs::create_dir_all(&options.appimages_dir)?;
    fs::create_dir_all(&options.applications_dir)?;

    let image_path = place_image(image, &options.appimages_dir)?;
    let icon_path = write_icon(&metadata, &options.icons_dir)?;

    let desktop = desktop_entry(&metadata, &image_path, icon_path.as_deref());
    let desktop_path = options.applications_dir.join(DESKTOP_ID);
    write_atomic(&desktop_path, desktop.to_string().as_bytes())?;

    if options.move_source && image_path != image {
        // Everything else already succeeded, so a failure here only leaves an
        // extra copy behind.
        if let Err(error) = fs::remove_file(image) {
            tracing::warn!(path = %image.display(), %error, "cannot remove the original AppImage");
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

/// Copies the image into `directory`, keeping its file name when possible.
fn place_image(source: &Path, directory: &Path) -> Result<PathBuf> {
    let source = fs::canonicalize(source)?;
    let directory = fs::canonicalize(directory)?;

    if source.parent() == Some(directory.as_path()) {
        return Ok(source);
    }

    let name = source
        .file_name()
        .ok_or_else(|| Error::InvalidName(source.clone()))?;
    let destination = unique_destination(&directory, name);
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

/// Finds a free file name, appending `-2`, `-3`… when the name is taken.
fn unique_destination(directory: &Path, name: &OsStr) -> PathBuf {
    let candidate = directory.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(name);
    let stem = path.file_stem().map_or_else(
        || "appimage".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );
    let extension = path.extension().map_or_else(String::new, |extension| {
        format!(".{}", extension.to_string_lossy())
    });

    (2..=9999)
        .map(|index| directory.join(format!("{stem}-{index}{extension}")))
        .find(|candidate| !candidate.exists())
        .unwrap_or_else(|| directory.join(format!("{stem}-{}", std::process::id())))
}

/// Writes the icon shipped by the AppImage into the `hicolor` theme.
fn write_icon(metadata: &AppMetadata, icons_dir: &Path) -> Result<Option<PathBuf>> {
    let Some(icon) = &metadata.icon else {
        return Ok(None);
    };

    let extension = Path::new(&icon.file_name)
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("png");
    let size = if extension.eq_ignore_ascii_case("svg") {
        "scalable"
    } else {
        "512x512"
    };

    let directory = icons_dir.join(size).join("apps");
    fs::create_dir_all(&directory)?;

    let path = directory.join(format!("{ICON_NAME}.{extension}"));
    write_atomic(&path, &icon.bytes)?;

    Ok(Some(path))
}

/// Builds the desktop entry, reusing the one shipped inside the AppImage.
fn desktop_entry(metadata: &AppMetadata, image: &Path, icon_path: Option<&Path>) -> DesktopFile {
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
    desktop.set("Desktop Entry", SELF_MARKER, "true");
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
        let desktop = desktop_entry(&metadata(), image, Some(icon)).to_string();

        assert!(desktop.contains("TryExec=/home/tester/AppImages/Gear LeverR.AppImage"));
        assert!(desktop.contains("Exec='/home/tester/AppImages/Gear LeverR.AppImage' %U"));
        assert!(desktop.contains(&format!("{SELF_MARKER}=true")));
        assert!(desktop.contains("X-AppImage-Version=0.1.0"));
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
        )
        .to_string();

        assert!(desktop.contains(&format!("X-AppImage-Version={}", env!("CARGO_PKG_VERSION"))));
        assert!(desktop.contains(&format!("Icon={ICON_NAME}")));
    }

    #[test]
    fn unique_names_keep_the_extension() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let name = OsStr::new("GearLeverR.AppImage");
        fs::write(directory.path().join(name), b"first").expect("first file");

        let destination = unique_destination(directory.path(), name);

        assert_eq!(
            destination.file_name(),
            Some(OsStr::new("GearLeverR-2.AppImage"))
        );
    }
}
