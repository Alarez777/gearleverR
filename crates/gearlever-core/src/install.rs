//! Integration of AppImages into the application menu.
//!
//! This mirrors what Gear Lever's `install_file` does: copy the AppImage into a
//! managed folder, extract and store its icon, and write a desktop entry with
//! `TryExec`, `Exec`, `Icon` and the `X-AppImage-*` keys, plus the per-app
//! configuration used by the update managers.

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::appimage::AppMetadata;
use crate::args;
use crate::config::Config;
use crate::desktop::DesktopFile;
use crate::error::{Error, Result};
use crate::scan::InstalledApp;

/// Environment variable the AppImage runtime uses to avoid double integration.
pub const DESKTOP_INTEGRATION_ENV: &str = "DESKTOPINTEGRATION=1";

/// What to do when an application with the same name exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// Keep the new file under a unique name.
    KeepBoth,
    /// Reuse the file names of the application being replaced.
    Replace,
}

/// Where the AppImage that is being replaced lives.
///
/// Only needed when [`Conflict::Replace`] is used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviousInstall {
    /// Absolute path of the old AppImage.
    pub image_path: PathBuf,
    /// Desktop entry of the old integration.
    pub desktop_path: PathBuf,
    /// Extra arguments stored in the old `Exec` line.
    pub exec_arguments: String,
    /// Environment variables stored in the old `Exec` line.
    pub env_variables: Vec<String>,
}

/// Options for integrating one AppImage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntegrateOptions {
    /// Folder that will hold the AppImage and its icons.
    pub appimages_dir: PathBuf,
    /// Folder that will hold the desktop entry.
    pub applications_dir: PathBuf,
    /// Conflict policy.
    pub conflict: Conflict,
    /// Remove the original file after a successful integration.
    pub move_source: bool,
    /// Name terminal applications after their executable.
    pub exec_name_for_terminal_apps: bool,
    /// Suffix appended to `Name`, used by the CLI to keep versions apart.
    pub name_suffix: Option<String>,
    /// Run `update-desktop-database` when it is installed.
    pub update_desktop_database: bool,
}

impl IntegrateOptions {
    /// Builds options with the defaults used by the graphical application.
    #[must_use]
    pub fn new(appimages_dir: impl Into<PathBuf>, applications_dir: impl Into<PathBuf>) -> Self {
        Self {
            appimages_dir: appimages_dir.into(),
            applications_dir: applications_dir.into(),
            conflict: Conflict::KeepBoth,
            move_source: true,
            exec_name_for_terminal_apps: false,
            name_suffix: None,
            update_desktop_database: true,
        }
    }
}

/// Integrates one AppImage and updates `config` with the new application.
///
/// The caller is expected to have validated that `source` is an AppImage, to
/// have extracted `metadata` from it, and to persist `config` afterwards.
///
/// # Errors
///
/// Returns [`Error::Io`] when the AppImage, icon or desktop entry cannot be
/// written, and [`Error::InvalidCommandLine`] when the embedded `Exec` line is
/// malformed enough to prevent quoting.
pub fn integrate(
    source: &Path,
    metadata: &AppMetadata,
    previous: Option<&PreviousInstall>,
    options: &IntegrateOptions,
    config: &mut Config,
) -> Result<InstalledApp> {
    fs::create_dir_all(&options.appimages_dir)?;
    fs::create_dir_all(&options.applications_dir)?;

    let (file_name, prefix) = destination_names(source, metadata, previous, options)?;
    let destination = options.appimages_dir.join(&file_name);

    if source != destination {
        fs::copy(source, &destination)?;
    }
    set_executable(&destination)?;

    let icon_path = store_icon(metadata, &options.appimages_dir, &prefix)?;
    let icon_value = icon_path.as_ref().map_or_else(
        || "applications-other".to_owned(),
        |path| path.to_string_lossy().into_owned(),
    );

    let default_arguments = args::parse(&metadata.exec)
        .map(|command| command.arguments_joined())
        .unwrap_or_default();

    let (exec_arguments, mut env_variables) =
        preserved_arguments(previous, options.conflict, &default_arguments, config);

    if !env_variables
        .iter()
        .any(|value| value.starts_with("DESKTOPINTEGRATION="))
    {
        env_variables.push(DESKTOP_INTEGRATION_ENV.to_owned());
    }

    let destination_text = destination.to_string_lossy();
    let mut desktop = DesktopFile::parse(&metadata.desktop_contents);

    if let Some(suffix) = &options.name_suffix {
        desktop.set(
            "Desktop Entry",
            "Name",
            format!("{} ({suffix})", metadata.name),
        );
    }

    desktop.set("Desktop Entry", "TryExec", destination_text.as_ref());
    desktop.set(
        "Desktop Entry",
        "Exec",
        exec_line(&destination, &env_variables, &exec_arguments),
    );
    desktop.set("Desktop Entry", "Icon", &icon_value);
    desktop.set("Desktop Entry", "X-AppImage-Name", &metadata.name);

    if let Some(version) = &metadata.version {
        desktop.set("Desktop Entry", "X-AppImage-Version", version);
    }

    let actions: Vec<String> = desktop
        .action_sections()
        .into_iter()
        .map(ToOwned::to_owned)
        .collect();
    for action in actions {
        let arguments = desktop
            .value(&action, "Exec")
            .and_then(|line| args::parse(line).ok())
            .map(|command| command.arguments)
            .unwrap_or_default();
        let action_exec = args::join(
            std::iter::once(destination_text.as_ref()).chain(arguments.iter().map(String::as_str)),
        );

        desktop.set(&action, "Exec", action_exec);
        desktop.set(&action, "Icon", &icon_value);
    }

    let desktop_id = format!("{prefix}.desktop").replace(' ', "_");
    let desktop_path = options.applications_dir.join(&desktop_id);
    fs::write(&desktop_path, desktop.to_string())?;

    let mut app_config: BTreeMap<String, String> = config.app(&destination);
    app_config.insert("name".to_owned(), metadata.name.clone());
    app_config.insert("file_path".to_owned(), destination_text.into_owned());
    app_config.insert("default_exec_arguments".to_owned(), default_arguments);
    config.set_app(&destination, app_config);

    if options.move_source && source != destination {
        fs::remove_file(source)?;
    }

    if options.update_desktop_database {
        refresh_desktop_database(&options.applications_dir);
    }

    Ok(InstalledApp {
        name: metadata.name.clone(),
        description: metadata.comment.clone(),
        version: metadata.version.clone(),
        terminal: metadata.terminal,
        size: fs::metadata(&destination)?.len(),
        image_path: destination,
        desktop_path,
        desktop_id,
        icon: Some(icon_value),
        icon_path,
        exec_arguments,
        env_variables,
        external: false,
    })
}

/// Computes the file name and desktop prefix for a new integration.
fn destination_names(
    source: &Path,
    metadata: &AppMetadata,
    previous: Option<&PreviousInstall>,
    options: &IntegrateOptions,
) -> Result<(String, String)> {
    if options.conflict == Conflict::Replace
        && let Some(previous) = previous
    {
        let file_name = previous
            .image_path
            .file_name()
            .ok_or_else(|| Error::InvalidName(previous.image_path.clone()))?
            .to_string_lossy()
            .into_owned();
        let prefix = previous
            .desktop_path
            .file_stem()
            .ok_or_else(|| Error::InvalidName(previous.desktop_path.clone()))?
            .to_string_lossy()
            .into_owned();

        return Ok((file_name, prefix));
    }

    let source_stem = source.file_stem().map_or_else(
        || "appimage".to_owned(),
        |stem| stem.to_string_lossy().into_owned(),
    );

    let mut base = if metadata.desktop_file.is_some() {
        metadata.name.to_lowercase().replace(' ', "_")
    } else {
        format!("gearlever_{source_stem}")
    };

    let mut extension = Some(".appimage");

    if metadata.desktop_file.is_some() && metadata.terminal && options.exec_name_for_terminal_apps {
        extension = None;
        if base == "appdir" {
            base = metadata.name.to_lowercase().replace(' ', "_");
        }
    }

    let mut stem = sanitize(&base);
    if extension.as_ref().is_some_and(|ext| stem.ends_with(ext)) {
        stem.truncate(stem.len() - ".appimage".len());
    }

    let existing = directory_names(&options.appimages_dir);
    let mut candidate = stem.clone();
    let mut counter = 0_u32;

    while existing.contains(&format!("{candidate}{}", extension.unwrap_or_default())) {
        candidate = if counter == 0 {
            match metadata
                .version
                .as_deref()
                .map(|version| version.replace('.', "_"))
            {
                Some(version) => format!("{stem}_{version}"),
                None => format!("{stem}_0"),
            }
        } else {
            format!("{stem}_{counter}")
        };
        counter += 1;
    }

    Ok((
        format!("{candidate}{}", extension.unwrap_or_default()),
        candidate,
    ))
}

/// Returns the custom arguments kept from the previous integration.
///
/// The original only preserves them when the user changed the arguments, that
/// is, when they differ from the defaults stored in the configuration.
fn preserved_arguments(
    previous: Option<&PreviousInstall>,
    conflict: Conflict,
    default_arguments: &str,
    config: &Config,
) -> (String, Vec<String>) {
    let Some(previous) = previous.filter(|_| conflict == Conflict::Replace) else {
        return (default_arguments.to_owned(), Vec::new());
    };

    let stored_default = config
        .app(&previous.image_path)
        .get("default_exec_arguments")
        .cloned()
        .unwrap_or_default();

    let arguments =
        if previous.exec_arguments.is_empty() || previous.exec_arguments == stored_default {
            default_arguments.to_owned()
        } else {
            previous.exec_arguments.clone()
        };

    (arguments, previous.env_variables.clone())
}

/// Builds the `Exec` value exactly like the original implementation.
fn exec_line(image: &Path, env_variables: &[String], arguments: &str) -> String {
    let mut command = String::new();

    if !env_variables.is_empty() {
        command.push_str("env ");
        command.push_str(&env_variables.join(" "));
        command.push(' ');
    }

    command.push_str(&args::quote(&image.to_string_lossy()));

    if !arguments.trim().is_empty() {
        command.push(' ');
        command.push_str(arguments);
    }

    command
}

/// Rewrites the `Exec` line of an integrated application.
///
/// Environment variables are written before the executable, and the arguments
/// are appended verbatim, matching what the original does when the user edits
/// them in the details view.
///
/// # Errors
///
/// Returns [`Error::Io`] when the desktop entry cannot be read or written.
pub fn set_exec(app: &InstalledApp, env_variables: &[String], arguments: &str) -> Result<()> {
    let contents = fs::read_to_string(&app.desktop_path)?;
    let mut desktop = DesktopFile::parse(&contents);
    desktop.set(
        "Desktop Entry",
        "Exec",
        exec_line(&app.image_path, env_variables, arguments),
    );
    fs::write(&app.desktop_path, desktop.to_string())?;

    Ok(())
}

fn store_icon(
    metadata: &AppMetadata,
    appimages_dir: &Path,
    prefix: &str,
) -> Result<Option<PathBuf>> {
    let Some(icon) = &metadata.icon else {
        return Ok(None);
    };

    let icons_dir = appimages_dir.join(".icons");
    fs::create_dir_all(&icons_dir)?;

    let extension = Path::new(&icon.file_name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("png");
    let path = icons_dir.join(format!("{prefix}.{extension}"));
    fs::write(&path, &icon.bytes)?;

    Ok(Some(path))
}

fn set_executable(path: &Path) -> Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)?;

    Ok(())
}

fn directory_names(directory: &Path) -> std::collections::BTreeSet<String> {
    let mut names = std::collections::BTreeSet::new();

    if let Ok(entries) = fs::read_dir(directory) {
        for entry in entries.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                names.insert(name.to_owned());
            }
        }
    }

    names
}

/// Removes characters that are not letters, digits, `_` or `.`.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .filter(|character| character.is_alphanumeric() || *character == '_' || *character == '.')
        .collect()
}

fn refresh_desktop_database(directory: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(directory)
        .arg("-q")
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::appimage::ExtractedIcon;

    fn fake_appimage(path: &Path) {
        let mut magic = vec![0_u8; 11];
        magic[0..8].copy_from_slice(b"\x7fELF\x02\x01\x01\x00");
        magic[8..11].copy_from_slice(b"AI\x02");
        fs::write(path, magic).unwrap();
    }

    fn metadata() -> AppMetadata {
        AppMetadata {
            name: "Test App".to_owned(),
            comment: "A test application".to_owned(),
            exec: "AppRun --embedded %U".to_owned(),
            try_exec: "AppRun".to_owned(),
            icon_name: Some("testapp".to_owned()),
            terminal: false,
            version: None,
            desktop_file: Some("/TestApp.desktop".to_owned()),
            desktop_contents: "\
[Desktop Entry]
Name=Test App
Comment=A test application
Exec=AppRun --embedded %U
Icon=testapp
Terminal=false
Actions=New;

[Desktop Action New]
Name=New Window
Exec=AppRun --new-window %U
"
            .to_owned(),
            icon: Some(ExtractedIcon {
                file_name: "testapp.png".to_owned(),
                bytes: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            }),
        }
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        paths: crate::paths::Paths,
        source: PathBuf,
        options: IntegrateOptions,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::with_home(dir.path());
        fs::create_dir_all(paths.default_appimages_dir()).unwrap();
        let source = dir.path().join("downloads").join("Test.AppImage");
        fs::create_dir_all(source.parent().unwrap()).unwrap();
        fake_appimage(&source);

        let mut options =
            IntegrateOptions::new(paths.default_appimages_dir(), paths.applications_dir());
        options.move_source = false;
        options.update_desktop_database = false;

        Fixture {
            _dir: dir,
            paths,
            source,
            options,
        }
    }

    #[test]
    fn writes_image_icon_desktop_entry_and_config() {
        let fixture = fixture();
        let mut config = Config::default();

        let app = integrate(
            &fixture.source,
            &metadata(),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        let image = fixture
            .paths
            .default_appimages_dir()
            .join("test_app.appimage");
        assert!(image.exists());
        assert_eq!(app.image_path, image);
        assert_ne!(
            fs::metadata(&image).unwrap().permissions().mode() & 0o111,
            0,
            "the AppImage must be executable"
        );

        let icon = fixture
            .paths
            .default_appimages_dir()
            .join(".icons/test_app.png");
        assert!(icon.exists());

        let desktop_path = fixture.paths.applications_dir().join("test_app.desktop");
        let contents = fs::read_to_string(&desktop_path).unwrap();
        let desktop = DesktopFile::parse(&contents);

        assert_eq!(
            desktop.value("Desktop Entry", "TryExec"),
            Some(image.to_string_lossy().as_ref())
        );
        assert_eq!(
            desktop.value("Desktop Entry", "Exec"),
            Some(format!("env DESKTOPINTEGRATION=1 {} --embedded %U", image.display()).as_str())
        );
        assert_eq!(
            desktop.value("Desktop Entry", "Icon"),
            Some(icon.to_string_lossy().as_ref())
        );
        assert_eq!(
            desktop.value("Desktop Entry", "X-AppImage-Name"),
            Some("Test App")
        );
        assert_eq!(
            desktop.value("Desktop Action New", "Exec"),
            Some(format!("{} --new-window %U", image.display()).as_str())
        );
        assert_eq!(
            desktop.value("Desktop Action New", "Icon"),
            Some(icon.to_string_lossy().as_ref())
        );

        assert_eq!(app.exec_arguments, "--embedded %U");
        assert_eq!(app.env_variables, vec![DESKTOP_INTEGRATION_ENV]);

        assert_eq!(
            config
                .app(&image)
                .get("default_exec_arguments")
                .map(String::as_str),
            Some("--embedded %U")
        );
        assert_eq!(
            config.app(&image).get("name").map(String::as_str),
            Some("Test App")
        );
    }

    #[test]
    fn keeps_both_versions_apart_without_double_extensions() {
        let fixture = fixture();
        let mut config = Config::default();

        integrate(
            &fixture.source,
            &metadata(),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();
        integrate(
            &fixture.source,
            &metadata(),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        let directory = fixture.paths.default_appimages_dir();
        assert!(directory.join("test_app.appimage").exists());
        assert!(directory.join("test_app_0.appimage").exists());
        assert!(!directory.join("test_app.appimage_0.appimage").exists());
    }

    #[test]
    fn uses_the_version_for_the_first_conflict() {
        let fixture = fixture();
        let mut config = Config::default();
        let mut versioned = metadata();
        versioned.version = Some("1.2.3".to_owned());

        integrate(
            &fixture.source,
            &versioned,
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();
        integrate(
            &fixture.source,
            &versioned,
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        assert!(
            fixture
                .paths
                .default_appimages_dir()
                .join("test_app_1_2_3.appimage")
                .exists()
        );
    }

    #[test]
    fn replace_reuses_names_and_custom_arguments() {
        let fixture = fixture();
        let mut config = Config::default();

        let first = integrate(
            &fixture.source,
            &metadata(),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        config.set_app(
            &first.image_path,
            BTreeMap::from([
                ("name".to_owned(), "Test App".to_owned()),
                (
                    "default_exec_arguments".to_owned(),
                    "--embedded %U".to_owned(),
                ),
            ]),
        );

        let previous = PreviousInstall {
            image_path: first.image_path.clone(),
            desktop_path: first.desktop_path.clone(),
            exec_arguments: "--custom value".to_owned(),
            env_variables: vec!["CUSTOM=1".to_owned()],
        };

        let mut options = fixture.options.clone();
        options.conflict = Conflict::Replace;
        options.move_source = true;

        let replaced = integrate(
            &fixture.source,
            &metadata(),
            Some(&previous),
            &options,
            &mut config,
        )
        .unwrap();

        assert_eq!(replaced.image_path, first.image_path);
        assert_eq!(replaced.desktop_path, first.desktop_path);

        let desktop = DesktopFile::parse(&fs::read_to_string(&replaced.desktop_path).unwrap());
        let exec = desktop.value("Desktop Entry", "Exec").unwrap();
        assert!(
            exec.contains("CUSTOM=1"),
            "environment is preserved: {exec}"
        );
        assert!(
            exec.ends_with("--custom value"),
            "custom arguments win: {exec}"
        );
    }

    #[test]
    fn moves_the_source_when_requested() {
        let mut fixture = fixture();
        fixture.options.move_source = true;
        let mut config = Config::default();

        integrate(
            &fixture.source,
            &metadata(),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        assert!(!fixture.source.exists(), "original file is removed");
    }

    #[test]
    fn set_exec_rewrites_environment_and_arguments() {
        let fixture = fixture();
        let mut config = Config::default();
        let app = integrate(
            &fixture.source,
            &metadata(),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        set_exec(&app, &["A=1".to_owned()], "--custom value").unwrap();

        let desktop = DesktopFile::parse(&fs::read_to_string(&app.desktop_path).unwrap());
        let exec = desktop.value("Desktop Entry", "Exec").unwrap();
        assert!(exec.starts_with("env A=1 "), "environment: {exec}");
        assert!(exec.ends_with("--custom value"), "arguments: {exec}");
        assert!(exec.contains("test_app.appimage"), "image path: {exec}");
    }

    #[test]
    fn sanitizes_names_and_avoids_double_extension() {
        assert_eq!(sanitize("my app (beta)"), "myappbeta");
        assert_eq!(sanitize("a_b.c-d"), "a_b.cd");
    }
}
