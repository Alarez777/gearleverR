//! Integration tests for the command line interface.
//!
//! The AppImage fixture is opt-in so the suite stays portable:
//!
//! ```sh
//! GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test -p gearlever
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_gearlever-r");

fn home() -> TempDir {
    tempfile::tempdir().expect("temporary home")
}

fn run(home: &Path, arguments: &[&str]) -> Output {
    Command::new(BIN)
        .args(arguments)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_STATE_HOME")
        .env("RUST_LOG", "error")
        .output()
        .expect("the binary must run")
}

fn test_appimage() -> Option<PathBuf> {
    std::env::var_os("GEARLEVER_TEST_APPIMAGE").map(PathBuf::from)
}

#[test]
fn lists_every_update_manager() {
    let directory = home();
    let output = run(directory.path(), &["--list-update-managers"]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    for name in [
        "StaticFileUpdater",
        "GithubUpdater",
        "GitlabUpdater",
        "CodebergUpdater",
        "FTPUpdater",
        "ForgejoUpdater",
    ] {
        assert!(stdout.contains(name), "missing {name} in {stdout}");
    }
}

#[test]
fn empty_list_is_a_versioned_json_document() {
    let directory = home();
    let output = run(directory.path(), &["--list-installed", "--json"]);

    assert!(output.status.success());
    let document: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");

    assert_eq!(document["schema_version"], 1);
    assert_eq!(document["installed"].as_array().map(Vec::len), Some(0));
}

#[test]
fn integrates_lists_and_removes_a_real_appimage() {
    let Some(source) = test_appimage() else {
        return;
    };

    let directory = home();
    let source_copy = directory.path().join("fixture.AppImage");
    fs::copy(&source, &source_copy).expect("copy the fixture");

    let source_arg = source_copy.to_string_lossy().into_owned();
    let integrated = run(directory.path(), &["--integrate", &source_arg, "-y"]);
    assert!(
        integrated.status.success(),
        "integrate failed: {}",
        String::from_utf8_lossy(&integrated.stderr)
    );

    let appimages = directory.path().join("AppImages");
    let images: Vec<PathBuf> = fs::read_dir(&appimages)
        .expect("AppImages exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "appimage")
        })
        .collect();
    assert_eq!(images.len(), 1, "exactly one integrated image");

    let listed = run(directory.path(), &["--list-installed", "--json"]);
    assert!(listed.status.success());
    let document: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    let installed = document["installed"].as_array().unwrap();
    assert_eq!(installed.len(), 1);
    assert_eq!(
        installed[0]["path"].as_str(),
        Some(images[0].to_string_lossy().as_ref())
    );
    assert!(installed[0]["desktop_id"].as_str().is_some());

    // A byte-identical copy opened from another folder is the same integration.
    let copies = directory.path().join("dist");
    fs::create_dir_all(&copies).expect("dist directory");
    let copy = copies.join("fixture-copy.AppImage");
    fs::copy(&images[0], &copy).expect("copy the integrated image");
    let copy_arg = copy.to_string_lossy().into_owned();

    let repeated = run(directory.path(), &["--integrate", &copy_arg, "-y"]);
    assert!(
        repeated.status.success(),
        "second integrate failed: {}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(
        String::from_utf8_lossy(&repeated.stdout).contains("already integrated"),
        "stdout: {}",
        String::from_utf8_lossy(&repeated.stdout)
    );

    let images_after: Vec<PathBuf> = fs::read_dir(&appimages)
        .expect("AppImages exists")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "appimage")
        })
        .collect();
    assert_eq!(images_after.len(), 1, "no duplicate integration");
    assert!(copy.exists(), "the outside copy is not moved");

    let image_arg = images[0].to_string_lossy().into_owned();
    let removed = run(directory.path(), &["--remove", &image_arg, "-y"]);
    assert!(removed.status.success());
    assert!(!images[0].exists(), "the AppImage is removed");

    let after = run(directory.path(), &["--list-installed", "--json"]);
    let document: serde_json::Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(document["installed"].as_array().map(Vec::len), Some(0));
}
