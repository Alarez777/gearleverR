//! End-to-end integration tests with a real AppImage.
//!
//! Opt-in so the suite stays portable:
//!
//! ```sh
//! GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test -p gearlever-core
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use gearlever_core::Result;
use gearlever_core::appimage::AppImage;
use gearlever_core::config::Config;
use gearlever_core::hash::HashCache;
use gearlever_core::install::{Conflict, IntegrateOptions, integrate};
use gearlever_core::paths::Paths;
use gearlever_core::scan::{ScanOptions, scan};
use gearlever_core::self_install::{self, InstallOptions};
use gearlever_core::uninstall::{Removal, uninstall};
use gearlever_core::update::{
    self, DownloadContext, ManagerKind, UpdateJob, UpdateManager, UpdateOptions, UpdateStatus,
};

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

/// A manager that serves a fixed AppImage from the local filesystem.
#[derive(Debug)]
struct CopyManager {
    source: PathBuf,
    config: BTreeMap<String, String>,
}

fn temporary_path(path: &Path) -> PathBuf {
    path.file_name().map_or_else(
        || path.to_path_buf(),
        |name| path.with_file_name(format!("{}.tmp", name.to_string_lossy())),
    )
}

impl UpdateManager for CopyManager {
    fn kind(&self) -> ManagerKind {
        ManagerKind::StaticFile
    }

    fn embedded(&self) -> Option<&str> {
        None
    }

    fn config(&self) -> &BTreeMap<String, String> {
        &self.config
    }

    fn check(&self, _hashes: &HashCache) -> Result<UpdateStatus> {
        Ok(UpdateStatus::Available)
    }

    fn download(&self, context: &mut DownloadContext<'_>) -> Result<PathBuf> {
        fs::create_dir_all(&context.directory)?;
        let destination = context.output_path();
        fs::copy(&self.source, &destination)?;
        (context.progress)(1.0);
        Ok(destination)
    }
}

#[test]
fn updates_two_appimages_in_one_batch() {
    let Some(source) = test_appimage() else {
        return;
    };

    let directory = tempfile::tempdir().expect("temporary home");
    let paths = Paths::with_home(directory.path());
    let metadata = AppImage::open(&source)
        .expect("fixture is a valid AppImage")
        .metadata()
        .expect("fixture has metadata");

    let mut integrate_options =
        IntegrateOptions::new(paths.default_appimages_dir(), paths.applications_dir());
    integrate_options.move_source = false;
    integrate_options.update_desktop_database = false;
    integrate_options.conflict = Conflict::KeepBoth;

    let mut config = Config::default();
    let first = integrate(&source, &metadata, None, &integrate_options, &mut config)
        .expect("first integration succeeds");
    let second = integrate(&source, &metadata, None, &integrate_options, &mut config)
        .expect("second integration succeeds");
    assert_ne!(first.image_path, second.image_path);

    let update_options = UpdateOptions {
        appimages_dir: integrate_options.appimages_dir.clone(),
        applications_dir: integrate_options.applications_dir.clone(),
        exec_name_for_terminal_apps: false,
        update_desktop_database: false,
        allow_unsafe_extractor: false,
        use_zsync_delta: false,
    };
    let jobs: Vec<UpdateJob> = [first, second]
        .into_iter()
        .map(|app| {
            UpdateJob::new(
                app,
                Arc::new(CopyManager {
                    source: source.clone(),
                    config: BTreeMap::new(),
                }),
            )
        })
        .collect();

    let report = update::apply_batch(
        &jobs,
        &update_options,
        &mut config,
        &update_options.staging_dir(),
        update::DEFAULT_PARALLEL_DOWNLOADS,
        &Arc::new(AtomicBool::new(false)),
        &mut |_| {},
    );

    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert!(!report.cancelled);
    assert_eq!(report.installed.len(), 2);

    for app in &report.installed {
        assert!(app.image_path.exists(), "the new AppImage is installed");
        assert!(app.desktop_path.exists(), "the desktop entry is rewritten");
        assert!(
            !temporary_path(&app.image_path).exists(),
            "no temporary file is left behind"
        );
    }
}
