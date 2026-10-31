//! End-to-end integration tests with a real AppImage.
//!
//! Opt-in so the suite stays portable:
//!
//! ```sh
//! GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test -p gearlever-core
//! ```

use std::path::PathBuf;

use gearlever_core::appimage::AppImage;
use gearlever_core::config::Config;
use gearlever_core::install::{IntegrateOptions, integrate};
use gearlever_core::paths::Paths;
use gearlever_core::scan::{ScanOptions, scan};
use gearlever_core::self_install::{self, InstallOptions};
use gearlever_core::uninstall::{Removal, uninstall};

fn test_appimage() -> Option<PathBuf> {
    std::env::var_os("GEARLEVER_TEST_APPIMAGE").map(PathBuf::from)
}

#[test]
fn integrates_scans_and_removes_a_real_appimage() {
    let Some(source) = test_appimage() else {
        return;
    };

    let directory = tempfile::tempdir().expect("temporary home");
    let paths = Paths::with_home(directory.path());
    let metadata = AppImage::open(&source)
        .expect("fixture is a valid AppImage")
        .metadata()
        .expect("fixture has metadata");

    let mut options =
        IntegrateOptions::new(paths.default_appimages_dir(), paths.applications_dir());
    options.move_source = false;
    options.update_desktop_database = false;

    let mut config = Config::default();
    let installed =
        integrate(&source, &metadata, None, &options, &mut config).expect("integration succeeds");

    assert!(installed.image_path.exists(), "the AppImage is copied");
    assert!(
        installed.desktop_path.exists(),
        "the desktop entry is written"
    );
    assert_eq!(
        config
            .app(&installed.image_path)
            .get("name")
            .map(String::as_str),
        Some(metadata.name.as_str())
    );

    let apps = scan(
        &paths,
        &ScanOptions {
            appimages_dir: &options.appimages_dir,
            include_external: true,
        },
    )
    .expect("scan succeeds");

    assert!(
        apps.iter()
            .any(|app| app.image_path == installed.image_path),
        "the new integration is listed"
    );

    uninstall(&installed, Removal::Delete, &mut config).expect("removal succeeds");

    assert!(!installed.image_path.exists());
    assert!(!installed.desktop_path.exists());
    assert!(config.app(&installed.image_path).is_empty());
}

#[test]
fn installs_itself_lists_itself_and_uninstalls_cleanly() {
    let Some(source) = test_appimage() else {
        return;
    };

    let directory = tempfile::tempdir().expect("temporary home");
    let paths = Paths::with_home(directory.path());
    let mut options = InstallOptions::new(&paths, paths.default_appimages_dir());
    options.move_source = false;
    options.update_desktop_database = false;

    let installation =
        self_install::install(&source, &options).expect("the self installation succeeds");

    assert!(installation.image_path.exists(), "the AppImage is copied");
    assert!(
        installation.desktop_path.exists(),
        "the desktop entry is written"
    );
    assert!(self_install::is_installed(&paths));

    let apps = scan(
        &paths,
        &ScanOptions {
            appimages_dir: &options.appimages_dir,
            include_external: true,
        },
    )
    .expect("scan succeeds");

    assert_eq!(apps.len(), 1, "Gear LeverR lists its own integration");
    assert!(apps[0].self_entry);
    assert!(apps[0].version.is_some(), "the entry declares a version");

    let app = apps.into_iter().next().expect("the self entry is listed");
    let mut config = Config::default();
    uninstall(&app, Removal::Delete, &mut config).expect("removal succeeds");

    assert!(!installation.image_path.exists());
    assert!(!installation.desktop_path.exists());
    if let Some(icon) = installation.icon_path {
        assert!(!icon.exists(), "the icon is removed");
    }
    assert!(!self_install::is_installed(&paths));
}
