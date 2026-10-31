//! Removal of integrated AppImages.

use std::fs;
use std::path::PathBuf;

use crate::config::Config;
use crate::error::{Error, Result};
use crate::scan::InstalledApp;

/// How to remove the AppImage file from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Removal {
    /// Move it to the freedesktop trash.
    Trash,
    /// Delete it immediately.
    Delete,
}

/// Removes an integrated application.
///
/// The AppImage goes to the trash unless [`Removal::Delete`] is used, then the
/// desktop entry and the icon are deleted and the configuration is cleaned up.
/// Missing files are ignored so a partially removed app can be cleaned again.
///
/// # Errors
///
/// Returns [`Error::Io`] when a file exists but cannot be removed.
pub fn uninstall(
    app: &InstalledApp,
    removal: Removal,
    config: &mut Config,
) -> Result<Vec<PathBuf>> {
    let mut removed = Vec::new();

    match removal {
        Removal::Delete => {
            remove_if_exists(&app.image_path)?;
        }
        Removal::Trash => match trash::delete(&app.image_path) {
            Ok(()) => {}
            Err(error) => {
                tracing::warn!(
                    path = %app.image_path.display(),
                    %error,
                    "cannot trash the AppImage, deleting it instead"
                );
                remove_if_exists(&app.image_path)?;
            }
        },
    }
    if !app.image_path.exists() {
        removed.push(app.image_path.clone());
    }

    remove_if_exists(&app.desktop_path)?;
    if !app.desktop_path.exists() {
        removed.push(app.desktop_path.clone());
    }

    if let Some(icon) = &app.icon_path {
        remove_if_exists(icon)?;
        if !icon.exists() {
            removed.push(icon.clone());
        }
    }

    config.delete_app(&app.image_path);

    Ok(removed)
}

fn remove_if_exists(path: &std::path::Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Paths;
    use std::collections::BTreeMap;

    #[test]
    fn deletes_files_and_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let paths = Paths::with_home(directory.path());
        let appimages = paths.default_appimages_dir();
        fs::create_dir_all(appimages.join(".icons")).unwrap();
        fs::create_dir_all(paths.applications_dir()).unwrap();

        let image = appimages.join("example.appimage");
        let icon = appimages.join(".icons/example.png");
        let desktop = paths.applications_dir().join("example.desktop");
        fs::write(&image, b"image").unwrap();
        fs::write(&icon, b"icon").unwrap();
        fs::write(&desktop, b"desktop").unwrap();

        let app = InstalledApp {
            name: "Example".to_owned(),
            description: String::new(),
            version: None,
            terminal: false,
            image_path: image.clone(),
            desktop_path: desktop.clone(),
            desktop_id: "example.desktop".to_owned(),
            icon: Some(icon.to_string_lossy().into_owned()),
            icon_path: Some(icon.clone()),
            exec_arguments: String::new(),
            env_variables: Vec::new(),
            size: 5,
            external: false,
            self_entry: false,
        };

        let mut config = Config::default();
        config.set_app(&image, BTreeMap::new());
        config.set_update_manager(&image, "GithubUpdater", BTreeMap::new());

        let removed = uninstall(&app, Removal::Delete, &mut config).unwrap();

        assert!(!image.exists());
        assert!(!desktop.exists());
        assert!(!icon.exists());
        assert!(removed.contains(&image));
        assert!(config.app(&image).is_empty());
        assert!(config.update_manager(&image).is_empty());
    }

    #[test]
    fn is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let app = InstalledApp {
            name: "Gone".to_owned(),
            description: String::new(),
            version: None,
            terminal: false,
            image_path: directory.path().join("gone.appimage"),
            desktop_path: directory.path().join("gone.desktop"),
            desktop_id: "gone.desktop".to_owned(),
            icon: None,
            icon_path: None,
            exec_arguments: String::new(),
            env_variables: Vec::new(),
            size: 0,
            external: false,
            self_entry: false,
        };

        let mut config = Config::default();
        uninstall(&app, Removal::Delete, &mut config).unwrap();
        uninstall(&app, Removal::Delete, &mut config).unwrap();
    }
}
