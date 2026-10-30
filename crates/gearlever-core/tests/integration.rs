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
