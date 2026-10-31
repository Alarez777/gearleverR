//! Discovery of AppImages that are already integrated into the desktop.

use std::fs;
use std::path::{Path, PathBuf};

use crate::appimage::detect;
use crate::args;
use crate::desktop::DesktopFile;
use crate::error::Result;
use crate::paths::Paths;

/// An AppImage integrated into the application menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledApp {
    /// `Name` from the desktop entry.
    pub name: String,
    /// `Comment` from the desktop entry, empty when missing.
    pub description: String,
    /// `X-AppImage-Version`, when present.
    pub version: Option<String>,
    /// Absolute path to the AppImage.
    pub image_path: PathBuf,
    /// Desktop entry that integrates the AppImage.
    pub desktop_path: PathBuf,
    /// File name of the desktop entry, useful for `gtk-launch`.
    pub desktop_id: String,
    /// Raw `Icon` value: an absolute path or a themed icon name.
    pub icon: Option<String>,
    /// Icon path when `Icon` is an absolute file.
    pub icon_path: Option<PathBuf>,
    /// Extra arguments stored in `Exec`, joined with spaces.
    pub exec_arguments: String,
    /// `KEY=value` prefixes stored in `Exec`.
    pub env_variables: Vec<String>,
    /// Whether the desktop entry runs the app in a terminal.
    pub terminal: bool,
    /// Size of the AppImage in bytes.
    pub size: u64,
    /// Whether the AppImage lives outside the managed folder.
    pub external: bool,
    /// Whether this is Gear LeverR's own integration.
    pub self_entry: bool,
}

/// Options that affect which integrations are returned.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions<'a> {
    /// Folder where Gear LeverR keeps AppImages.
    pub appimages_dir: &'a Path,
    /// Whether integrations that point outside `appimages_dir` are listed.
    pub include_external: bool,
}

/// Lists the AppImages integrated in `$XDG_DATA_HOME/applications`.
///
/// The scan never hashes an AppImage: it validates the runtime magic, reads the
/// desktop entry and stats the file. Results are sorted case-insensitively by
/// name.
///
/// # Errors
///
/// Returns [`crate::Error::Io`] only when the applications directory exists but
/// cannot be read; individual unreadable entries are skipped.
pub fn scan(paths: &Paths, options: &ScanOptions<'_>) -> Result<Vec<InstalledApp>> {
    let directory = paths.applications_dir();

    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };

    let mut apps = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "desktop")
        {
            continue;
        }

        if let Some(app) = read_entry(&path, options) {
            apps.push(app);
        }
    }

    apps.sort_by(|left, right| {
        left.name
            .to_lowercase()
            .cmp(&right.name.to_lowercase())
            .then_with(|| left.image_path.cmp(&right.image_path))
    });

    Ok(apps)
}

/// Reads one desktop entry, returning `None` when it is not our kind of app.
fn read_entry(path: &Path, options: &ScanOptions<'_>) -> Option<InstalledApp> {
    let contents = fs::read_to_string(path).ok()?;
    let desktop = DesktopFile::parse(&contents);
    let main = desktop.section("Desktop Entry")?;

    let self_entry = main
        .get(crate::self_install::SELF_MARKER)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));

    let try_exec = main
        .get("TryExec")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())?;
    let image_path = PathBuf::from(try_exec);

    if !image_path.is_file() || detect::image_type(&image_path).is_err() {
        return None;
    }

    let command = args::parse(main.get("Exec")?).ok()?;

    // The original compares against the file name inside the managed folder, so
    // an app is "external" only when no file with that name is managed.
    let external = image_path
        .file_name()
        .is_none_or(|name| !options.appimages_dir.join(name).is_file());

    if external && !options.include_external {
        return None;
    }

    let icon = main
        .get("Icon")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    let icon_path = icon
        .as_deref()
        .map(Path::new)
        .filter(|candidate| candidate.is_absolute())
        .map(Path::to_path_buf)
        .filter(|candidate| candidate.is_file());

    let name = main
        .get("Name")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map_or_else(
            || {
                image_path.file_stem().map_or_else(
                    || "Unknown".to_owned(),
                    |stem| stem.to_string_lossy().into_owned(),
                )
            },
            ToOwned::to_owned,
        );

    Some(InstalledApp {
        name,
        description: main.get("Comment").cloned().unwrap_or_default(),
        terminal: main
            .get("Terminal")
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        version: main
            .get("X-AppImage-Version")
            .filter(|value| !value.is_empty())
            .cloned(),
        size: fs::metadata(&image_path).map_or(0, |metadata| metadata.len()),
        desktop_id: path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        desktop_path: path.to_path_buf(),
        image_path,
        icon,
        icon_path,
        exec_arguments: command.arguments_joined(),
        env_variables: command.env_variables,
        external,
        self_entry,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_fake_appimage(path: &Path) {
        let mut magic = vec![0u8; 11];
        magic[0..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x00");
        magic[8..11].copy_from_slice(b"AI\x02");
        fs::write(path, magic).unwrap();
    }

    #[test]
    fn finds_integrated_appimages_and_parses_exec() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(directory.path());
        let appimages = paths.default_appimages_dir();
        fs::create_dir_all(&appimages).unwrap();
        fs::create_dir_all(paths.applications_dir()).unwrap();

        let image = appimages.join("example.appimage");
        write_fake_appimage(&image);
        fs::write(
            paths.applications_dir().join("example.desktop"),
            format!(
                "[Desktop Entry]\nName=Example\nComment=Demo\nTryExec={}\n\
                 Exec=env DESKTOPINTEGRATION=1 {} --flag %U\nX-AppImage-Version=1.0\nIcon=example\n",
                image.display(),
                image.display()
            ),
        )
        .unwrap();

        let apps = scan(
            &paths,
            &ScanOptions {
                appimages_dir: &appimages,
                include_external: true,
            },
        )
        .unwrap();

        assert_eq!(apps.len(), 1);
        let app = &apps[0];
        assert_eq!(app.name, "Example");
        assert_eq!(app.version.as_deref(), Some("1.0"));
        assert_eq!(app.exec_arguments, "--flag %U");
        assert_eq!(app.env_variables, vec!["DESKTOPINTEGRATION=1"]);
        assert!(!app.external);
        assert_eq!(app.desktop_id, "example.desktop");
    }

    #[test]
    fn hides_external_apps_when_requested() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(directory.path());
        let appimages = paths.default_appimages_dir();
        let other = directory.path().join("elsewhere");
        fs::create_dir_all(&appimages).unwrap();
        fs::create_dir_all(&other).unwrap();
        fs::create_dir_all(paths.applications_dir()).unwrap();

        let image = other.join("external.appimage");
        write_fake_appimage(&image);
        fs::write(
            paths.applications_dir().join("external.desktop"),
            format!(
                "[Desktop Entry]\nName=External\nTryExec={}\nExec={}\n",
                image.display(),
                image.display()
            ),
        )
        .unwrap();

        let hidden = scan(
            &paths,
            &ScanOptions {
                appimages_dir: &appimages,
                include_external: false,
            },
        )
        .unwrap();
        assert!(hidden.is_empty());

        let shown = scan(
            &paths,
            &ScanOptions {
                appimages_dir: &appimages,
                include_external: true,
            },
        )
        .unwrap();
        assert_eq!(shown.len(), 1);
        assert!(shown[0].external);
    }

    #[test]
    fn lists_the_self_installed_desktop_entry() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(directory.path());
        let appimages = paths.default_appimages_dir();
        fs::create_dir_all(&appimages).unwrap();
        fs::create_dir_all(paths.applications_dir()).unwrap();

        let image = appimages.join("GearLeverR.AppImage");
        write_fake_appimage(&image);
        fs::write(
            paths
                .applications_dir()
                .join(crate::self_install::DESKTOP_ID),
            format!(
                "[Desktop Entry]\nName=Gear LeverR\nTryExec={}\nExec={}\n{}={}\n",
                image.display(),
                image.display(),
                crate::self_install::SELF_MARKER,
                "true"
            ),
        )
        .unwrap();

        let apps = scan(
            &paths,
            &ScanOptions {
                appimages_dir: &appimages,
                include_external: true,
            },
        )
        .unwrap();

        assert_eq!(apps.len(), 1);
        assert!(apps[0].self_entry);
    }

    #[test]
    fn ignores_desktop_entries_that_do_not_point_to_an_appimage() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(directory.path());
        let appimages = paths.default_appimages_dir();
        fs::create_dir_all(paths.applications_dir()).unwrap();

        fs::write(
            paths.applications_dir().join("other.desktop"),
            "[Desktop Entry]\nName=Other\nTryExec=/bin/sh\nExec=/bin/sh\n",
        )
        .unwrap();

        let apps = scan(
            &paths,
            &ScanOptions {
                appimages_dir: &appimages,
                include_external: true,
            },
        )
        .unwrap();
        assert!(apps.is_empty());
    }
}
