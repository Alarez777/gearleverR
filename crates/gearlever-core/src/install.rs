//! Integration of AppImages into the application menu.
//!
//! This mirrors what Gear Lever's `install_file` does: copy the AppImage into a
//! managed folder, extract and store its icon, and write a desktop entry with
//! `TryExec`, `Exec`, `Icon` and the `X-AppImage-*` keys, plus the per-app
//! configuration used by the update managers.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::appimage::AppMetadata;
use crate::args;
use crate::config::Config;
use crate::desktop::DesktopFile;
use crate::error::{Error, Result};
use crate::hash::same_content;
use crate::scan::InstalledApp;

/// Environment variable the AppImage runtime uses to avoid double integration.
pub const DESKTOP_INTEGRATION_ENV: &str = "DESKTOPINTEGRATION=1";

/// What to do when an application with the same name exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Conflict {
    /// Keep the new file under a unique name.
    KeepBoth,
    /// Reuse the file names of the application being replaced.
    ///
    /// Gear LeverR's own integration is named after its build variant instead,
    /// so switching between the default and gtk-included builds updates the name.
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

/// Returns whether `candidate` is the integrated copy of `path`.
///
/// An exact path always counts. A candidate inside `appimages_dir` also matches
/// when its contents are byte-identical, no matter where `path` lives: opening
/// the same AppImage from `Downloads` or `dist` is reported as already
/// integrated, like Gear Lever does. Integrations outside the managed folder
/// only match by path.
#[must_use]
pub fn is_installed_copy(path: &Path, candidate: &Path, appimages_dir: &Path) -> bool {
    same_path(path, candidate)
        || (candidate
            .parent()
            .is_some_and(|parent| same_path(parent, appimages_dir))
            && same_content(path, candidate))
}

fn same_path(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
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
#[expect(
    clippy::too_many_lines,
    reason = "writing the image, desktop entry and configuration as one replacement reads better together"
)]
pub fn integrate(
    source: &Path,
    metadata: &AppMetadata,
    previous: Option<&PreviousInstall>,
    options: &IntegrateOptions,
    config: &mut Config,
) -> Result<InstalledApp> {
    fs::create_dir_all(&options.appimages_dir)?;
    fs::create_dir_all(&options.applications_dir)?;

    let self_variant = crate::self_install::is_self_metadata(metadata)
        .then(|| crate::self_install::detect_variant(metadata, source));
    let previous_entry = previous
        .filter(|_| options.conflict == Conflict::Replace)
        .map(previous_entry);

    let (file_name, prefix) = destination_names(source, metadata, previous, options, self_variant)?;
    let destination = options.appimages_dir.join(&file_name);

    if source != destination {
        copy_image(source, &destination)?;
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

    // Gear LeverR can replace its own integration from the file list; keep the
    // marker so `scan` and the self-removal dialog still recognise it. A self
    // build replacing a plain entry also becomes the canonical self entry.
    let replaces_self =
        options.conflict == Conflict::Replace && self_variant.is_some() && previous.is_some();
    if replaces_self
        || previous_entry
            .as_ref()
            .is_some_and(|entry| entry.self_entry)
    {
        desktop.set("Desktop Entry", crate::self_install::SELF_MARKER, "true");
    }

    if let Some(variant) = self_variant {
        desktop.set(
            "Desktop Entry",
            crate::self_install::VARIANT_KEY,
            variant.as_key(),
        );
    }

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

    if options.conflict == Conflict::Replace
        && self_variant.is_some()
        && let Some(previous) = previous
        && previous.image_path != destination
    {
        config.move_app(&previous.image_path, &destination);
    }

    let mut app_config: BTreeMap<String, String> = config.app(&destination);
    app_config.insert("name".to_owned(), metadata.name.clone());
    app_config.insert("file_path".to_owned(), destination_text.into_owned());
    app_config.insert("default_exec_arguments".to_owned(), default_arguments);
    config.set_app(&destination, app_config);

    if options.move_source && source != destination {
        match fs::remove_file(source) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }

    if options.conflict == Conflict::Replace
        && self_variant.is_some()
        && let Some(previous) = previous
        && let Some(previous_entry) = previous_entry.as_ref()
    {
        remove_replaced_self_artifacts(
            previous,
            previous_entry,
            &destination,
            &desktop_path,
            icon_path.as_deref(),
            &options.appimages_dir,
        );
    }

    if options.update_desktop_database {
        refresh_desktop_database(&options.applications_dir);
    }

    Ok(InstalledApp {
        name: metadata.name.clone(),
        appimage_name: Some(metadata.name.clone()),
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
        self_entry: false,
    })
}

/// Computes the file name and desktop prefix for a new integration.
fn destination_names(
    source: &Path,
    metadata: &AppMetadata,
    previous: Option<&PreviousInstall>,
    options: &IntegrateOptions,
    self_variant: Option<crate::self_install::Variant>,
) -> Result<(String, String)> {
    if options.conflict == Conflict::Replace
        && let Some(previous) = previous
        && let Some(variant) = self_variant
    {
        let file_name = unique_image_name(
            variant.image_stem(),
            metadata.version.as_deref(),
            &options.appimages_dir,
            &[previous.image_path.as_path(), source],
        );

        return Ok((file_name, crate::self_install::DESKTOP_STEM.to_owned()));
    }

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

    let mut base = if let Some(variant) = self_variant {
        variant.image_stem().to_owned()
    } else if metadata.desktop_file.is_some() {
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
    let extension = extension.unwrap_or_default();
    let candidate = unique_stem(&stem, metadata.version.as_deref(), extension, &existing);

    Ok((format!("{candidate}{extension}"), candidate))
}

/// Returns a free image name for `stem`, excluding the paths being replaced.
pub(crate) fn unique_image_name(
    stem: &str,
    version: Option<&str>,
    directory: &Path,
    excluded: &[&Path],
) -> String {
    let mut existing = directory_names(directory);

    for path in excluded {
        let path = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        if path
            .parent()
            .is_some_and(|parent| same_path(parent, directory))
            && let Some(name) = path.file_name()
        {
            existing.remove(name.to_string_lossy().as_ref());
        }
    }

    format!(
        "{}.appimage",
        unique_stem(stem, version, ".appimage", &existing)
    )
}

/// Appends `_version`, `_0`, `_1`… until the name is free.
fn unique_stem(
    stem: &str,
    version: Option<&str>,
    extension: &str,
    existing: &BTreeSet<String>,
) -> String {
    let mut candidate = stem.to_owned();
    let mut counter = 0_u32;

    while existing.contains(&format!("{candidate}{extension}")) {
        candidate = if counter == 0 {
            match version.map(|version| version.replace('.', "_")) {
                Some(version) => format!("{stem}_{version}"),
                None => format!("{stem}_0"),
            }
        } else {
            format!("{stem}_{counter}")
        };
        counter += 1;
    }

    candidate
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

/// Returns the destination of an update that replaces `previous`.
pub(crate) fn replacement_destination(
    previous: &PreviousInstall,
    appimages_dir: &Path,
) -> Result<PathBuf> {
    let file_name = previous
        .image_path
        .file_name()
        .ok_or_else(|| Error::InvalidName(previous.image_path.clone()))?;

    Ok(appimages_dir.join(file_name))
}

/// Moves a downloaded update to the temporary path used by [`integrate`].
///
/// When both paths live on the same filesystem the move is a rename; otherwise
/// the file is copied once and the temporary download is removed.
pub(crate) fn stage_download(source: &Path, destination: &Path) -> Result<()> {
    let temporary = temporary_path(destination);
    if source == temporary {
        return Ok(());
    }

    if let Some(parent) = temporary.parent() {
        fs::create_dir_all(parent)?;
    }

    match fs::rename(source, &temporary) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            let result = (|| -> Result<()> {
                fs::copy(source, &temporary)?;
                set_executable(&temporary)?;
                Ok(())
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
                return result;
            }

            let _ = fs::remove_file(source);
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// Copies an AppImage through a temporary file, then renames it into place.
///
/// Renaming over the destination works even when the old file is still running,
/// which happens when a manual update replaces the current version. When the
/// source is already the staging path used by updates, only the rename runs.
fn copy_image(source: &Path, destination: &Path) -> Result<()> {
    let temporary = temporary_path(destination);

    if source == temporary {
        let result = (|| -> Result<()> {
            set_executable(&temporary)?;
            fs::rename(&temporary, destination)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        return result;
    }

    let result = (|| -> Result<()> {
        fs::copy(source, &temporary)?;
        set_executable(&temporary)?;
        fs::rename(&temporary, destination)?;
        Ok(())
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }

    result
}

pub(crate) fn temporary_path(path: &Path) -> PathBuf {
    path.file_name().map_or_else(
        || path.to_path_buf(),
        |name| path.with_file_name(format!("{}.tmp", name.to_string_lossy())),
    )
}

#[derive(Debug, Default)]
struct PreviousEntry {
    self_entry: bool,
    icon: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy)]
enum ReplacedArtifact {
    AppImage,
    DesktopEntry,
    Icon,
}

impl ReplacedArtifact {
    const fn description(self) -> &'static str {
        match self {
            Self::AppImage => "AppImage",
            Self::DesktopEntry => "desktop entry",
            Self::Icon => "icon",
        }
    }
}

fn previous_entry(previous: &PreviousInstall) -> PreviousEntry {
    let Ok(contents) = fs::read_to_string(&previous.desktop_path) else {
        return PreviousEntry::default();
    };
    let desktop = DesktopFile::parse(&contents);

    let self_entry = desktop
        .value("Desktop Entry", crate::self_install::SELF_MARKER)
        .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    let icon = desktop
        .value("Desktop Entry", "Icon")
        .map(Path::new)
        .filter(|path| path.is_absolute() && path.is_file())
        .map(Path::to_path_buf);

    PreviousEntry { self_entry, icon }
}

fn remove_replaced_self_artifacts(
    previous: &PreviousInstall,
    previous_entry: &PreviousEntry,
    destination: &Path,
    desktop_path: &Path,
    icon_path: Option<&Path>,
    appimages_dir: &Path,
) {
    if previous.image_path != destination
        && previous
            .image_path
            .parent()
            .is_some_and(|parent| same_path(parent, appimages_dir))
    {
        remove_file_logged(&previous.image_path, ReplacedArtifact::AppImage);
    }

    if previous.desktop_path != desktop_path {
        remove_file_logged(&previous.desktop_path, ReplacedArtifact::DesktopEntry);
    }

    if let Some(old_icon) = &previous_entry.icon
        && icon_path != Some(old_icon.as_path())
    {
        remove_file_logged(old_icon, ReplacedArtifact::Icon);
    }
}

fn remove_file_logged(path: &Path, artifact: ReplacedArtifact) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let kind = artifact.description();
            tracing::warn!(path = %path.display(), %error, "cannot remove the old self {kind}");
        }
    }
}

fn directory_names(directory: &Path) -> BTreeSet<String> {
    let mut names = BTreeSet::new();

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

pub(crate) fn refresh_desktop_database(directory: &Path) {
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
        dir: tempfile::TempDir,
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
            dir,
            paths,
            source,
            options,
        }
    }

    fn self_metadata(variant: crate::self_install::Variant) -> AppMetadata {
        let mut metadata = metadata();
        metadata.name = "Gear LeverR".to_owned();
        metadata.desktop_file = Some(crate::self_install::DESKTOP_ID.to_owned());
        metadata.version = Some("0.1.0".to_owned());
        metadata.desktop_contents = format!(
            "[Desktop Entry]\nName=Gear LeverR\nExec=gearlever-r %U\nIcon={}\n{}={}\n",
            crate::self_install::ICON_NAME,
            crate::self_install::VARIANT_KEY,
            variant.as_key()
        );
        metadata.icon = Some(ExtractedIcon {
            file_name: "gearlever.png".to_owned(),
            bytes: vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        });
        metadata
    }

    fn self_previous(fixture: &Fixture, image: &Path, icon: &Path) -> PreviousInstall {
        let desktop_path = fixture
            .paths
            .applications_dir()
            .join(crate::self_install::DESKTOP_ID);
        fs::create_dir_all(fixture.paths.applications_dir()).unwrap();
        fs::write(
            &desktop_path,
            format!(
                "[Desktop Entry]\nName=Gear LeverR\nTryExec={}\nExec={} %U\nIcon={}\n{}={}\n{}={}\n",
                image.display(),
                image.display(),
                icon.display(),
                crate::self_install::SELF_MARKER,
                "true",
                crate::self_install::VARIANT_KEY,
                "gtk-included",
            ),
        )
        .unwrap();

        PreviousInstall {
            image_path: image.to_path_buf(),
            desktop_path,
            exec_arguments: "%U".to_owned(),
            env_variables: Vec::new(),
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
    fn replacing_itself_uses_the_variant_name_and_cleans_the_old_copy() {
        let fixture = fixture();
        let mut config = Config::default();
        let appimages = fixture.paths.default_appimages_dir();
        let old_image = appimages.join("GearLeverR-0.1.0-x86_64-gtk-included.AppImage");
        fake_appimage(&old_image);
        let old_icon = fixture.dir.path().join("old-icon.png");
        fs::write(&old_icon, b"icon").unwrap();
        let previous = self_previous(&fixture, &old_image, &old_icon);

        config.set_app(
            &old_image,
            BTreeMap::from([
                ("name".to_owned(), "Gear LeverR".to_owned()),
                ("default_exec_arguments".to_owned(), "%U".to_owned()),
                ("skip_updates".to_owned(), "true".to_owned()),
            ]),
        );
        config.set_update_manager(
            &old_image,
            "GithubUpdater",
            BTreeMap::from([("repo".to_owned(), "owner/repo".to_owned())]),
        );

        let mut options = fixture.options.clone();
        options.conflict = Conflict::Replace;
        options.move_source = true;

        let installed = integrate(
            &fixture.source,
            &self_metadata(crate::self_install::Variant::Default),
            Some(&previous),
            &options,
            &mut config,
        )
        .unwrap();

        let expected = appimages.join("gear_leverr.appimage");
        assert_eq!(installed.image_path, expected);
        assert!(!old_image.exists(), "the old AppImage is removed");
        assert!(!old_icon.exists(), "the old icon is removed");
        assert!(!fixture.source.exists(), "the downloaded image is moved");
        assert_eq!(
            installed.desktop_path,
            fixture
                .paths
                .applications_dir()
                .join(crate::self_install::DESKTOP_ID)
        );

        let desktop = DesktopFile::parse(&fs::read_to_string(&installed.desktop_path).unwrap());
        assert_eq!(
            desktop.value("Desktop Entry", crate::self_install::SELF_MARKER),
            Some("true")
        );
        assert_eq!(
            desktop.value("Desktop Entry", crate::self_install::VARIANT_KEY),
            Some("default")
        );

        assert!(config.app(&old_image).is_empty());
        assert_eq!(
            config
                .app(&expected)
                .get("skip_updates")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            config
                .update_manager(&expected)
                .get("manager")
                .map(String::as_str),
            Some("GithubUpdater")
        );
        assert!(config.update_manager(&old_image).is_empty());
    }

    #[test]
    fn switching_self_variants_uses_the_new_name() {
        let fixture = fixture();
        let mut config = Config::default();
        let appimages = fixture.paths.default_appimages_dir();
        let old_image = appimages.join("gear_leverr.appimage");
        fake_appimage(&old_image);
        let old_icon = fixture.dir.path().join("old-icon.png");
        fs::write(&old_icon, b"icon").unwrap();
        let previous = self_previous(&fixture, &old_image, &old_icon);

        let mut options = fixture.options.clone();
        options.conflict = Conflict::Replace;
        options.move_source = true;

        let installed = integrate(
            &fixture.source,
            &self_metadata(crate::self_install::Variant::GtkIncluded),
            Some(&previous),
            &options,
            &mut config,
        )
        .unwrap();

        assert_eq!(
            installed.image_path,
            appimages.join("gear_leverr_gtk_included.appimage")
        );
        assert!(!old_image.exists(), "the previous variant is removed");
    }

    #[test]
    fn keeping_both_self_variants_leaves_the_existing_entry_untouched() {
        let fixture = fixture();
        let mut config = Config::default();
        let appimages = fixture.paths.default_appimages_dir();
        let old_image = appimages.join("gear_leverr.appimage");
        fake_appimage(&old_image);

        let installed = integrate(
            &fixture.source,
            &self_metadata(crate::self_install::Variant::GtkIncluded),
            None,
            &fixture.options,
            &mut config,
        )
        .unwrap();

        assert_eq!(
            installed.image_path,
            appimages.join("gear_leverr_gtk_included.appimage")
        );
        assert!(old_image.exists(), "the existing self entry is untouched");
        assert_eq!(
            installed.desktop_path,
            fixture
                .paths
                .applications_dir()
                .join("gear_leverr_gtk_included.desktop")
        );

        let desktop = DesktopFile::parse(&fs::read_to_string(&installed.desktop_path).unwrap());
        assert_eq!(
            desktop.value("Desktop Entry", crate::self_install::SELF_MARKER),
            None
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

    #[test]
    fn detects_an_identical_copy_outside_the_managed_folder() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        let installed = managed.join("Installed.AppImage");
        let copy = downloads.join("Copy.AppImage");
        fs::write(&installed, b"same bytes").unwrap();
        fs::write(&copy, b"same bytes").unwrap();

        assert!(
            is_installed_copy(&copy, &installed, &managed),
            "a byte-identical copy opened from outside the managed folder is installed"
        );
    }

    #[test]
    fn detects_an_identical_copy_inside_the_managed_folder() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("AppImages");
        fs::create_dir_all(&managed).unwrap();
        let installed = managed.join("Installed.AppImage");
        let copy = managed.join("Copy.AppImage");
        fs::write(&installed, b"same bytes").unwrap();
        fs::write(&copy, b"same bytes").unwrap();

        assert!(
            is_installed_copy(&copy, &installed, &managed),
            "a copy in the managed folder is the installed AppImage"
        );
    }

    #[test]
    fn rejects_a_copy_with_different_contents() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        let installed = managed.join("Installed.AppImage");
        let copy = downloads.join("Copy.AppImage");
        fs::write(&installed, b"same bytes").unwrap();
        fs::write(&copy, b"different bytes").unwrap();

        assert!(
            !is_installed_copy(&copy, &installed, &managed),
            "different contents are a different AppImage"
        );
    }

    #[test]
    fn keeps_an_exact_path_outside_the_managed_folder() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        let outside = downloads.join("Installed.AppImage");
        fs::write(&outside, b"same bytes").unwrap();

        assert!(
            is_installed_copy(&outside, &outside, &managed),
            "the exact path always counts as installed"
        );
    }

    #[test]
    fn ignores_an_installed_copy_outside_the_managed_folder() {
        let directory = tempfile::tempdir().unwrap();
        let managed = directory.path().join("AppImages");
        let downloads = directory.path().join("Downloads");
        fs::create_dir_all(&managed).unwrap();
        fs::create_dir_all(&downloads).unwrap();
        let installed = downloads.join("Installed.AppImage");
        let opened = managed.join("Opened.AppImage");
        fs::write(&installed, b"same bytes").unwrap();
        fs::write(&opened, b"same bytes").unwrap();

        assert!(
            !is_installed_copy(&opened, &installed, &managed),
            "content only counts when the installed copy is managed"
        );
    }

    #[test]
    fn stages_a_download_and_renames_it_into_place() {
        let directory = tempfile::tempdir().unwrap();
        let appimages = directory.path().join("AppImages");
        fs::create_dir_all(&appimages).unwrap();
        let source = directory.path().join("download.AppImage");
        let destination = appimages.join("App.AppImage");
        fs::write(&source, b"new image").unwrap();

        stage_download(&source, &destination).unwrap();

        let temporary = temporary_path(&destination);
        assert!(!source.exists(), "the download is moved out of staging");
        assert!(temporary.exists(), "the image waits at the temporary path");

        copy_image(&temporary, &destination).unwrap();

        assert!(!temporary.exists(), "the temporary file is renamed");
        assert_eq!(fs::read(&destination).unwrap(), b"new image");
    }
}
