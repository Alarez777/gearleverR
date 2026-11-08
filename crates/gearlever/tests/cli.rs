//! Integration tests for the command line interface.
//!
//! The AppImage fixture is opt-in so the suite stays portable:
//!
//! ```sh
//! GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test -p gearlever
//! ```

use std::fs;
use std::io::{Read as _, Write as _};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use gearlever_core::appimage::ElfArch;
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

/// Serves `body` over HTTP until the returned flag is set.
fn serve(body: Vec<u8>) -> (String, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the test server");
    listener.set_nonblocking(true).expect("non-blocking server");
    let address = listener.local_addr().expect("server address");
    let stop = Arc::new(AtomicBool::new(false));

    let server = {
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                        let mut buffer = [0_u8; 4096];
                        let read = stream.read(&mut buffer).unwrap_or(0);
                        let request = String::from_utf8_lossy(&buffer[..read]);
                        let headers = format!(
                            "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = stream.write_all(headers.as_bytes());
                        if request.starts_with("GET ") {
                            let _ = stream.write_all(&body);
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        })
    };

    (format!("http://{address}/App.AppImage"), stop, server)
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
fn update_all_without_appimages_reports_no_updates() {
    let directory = home();
    let output = run(directory.path(), &["--update", "--all", "-y"]);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("No updates available"),
        "stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
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

#[test]
fn warns_when_integrating_a_foreign_appimage() {
    let Some(source) = test_appimage() else {
        return;
    };
    let Some(host) = ElfArch::host() else {
        return;
    };

    let directory = home();
    let source_copy = directory.path().join("foreign.AppImage");
    let mut bytes = fs::read(&source).expect("read the fixture");
    let (foreign_machine, foreign_name) = if host == ElfArch::X86_64 {
        (0xb7_u16, "aarch64")
    } else {
        (0x3e_u16, "x86_64")
    };
    bytes[0x12..0x14].copy_from_slice(&foreign_machine.to_le_bytes());
    fs::write(&source_copy, bytes).expect("write the patched fixture");

    let source_arg = source_copy.to_string_lossy().into_owned();
    let integrated = run(directory.path(), &["--integrate", &source_arg, "-y"]);

    assert!(
        integrated.status.success(),
        "integrate failed: {}",
        String::from_utf8_lossy(&integrated.stderr)
    );
    let stderr = String::from_utf8_lossy(&integrated.stderr);
    assert!(stderr.contains("Warning:"), "missing warning: {stderr}");
    assert!(
        stderr.contains(foreign_name),
        "missing architecture in warning: {stderr}"
    );
}

#[test]
fn updates_all_appimages_from_a_static_url() {
    let Some(source) = test_appimage() else {
        return;
    };

    let bytes = fs::read(&source).expect("read the fixture");
    let expected_size = u64::try_from(bytes.len()).expect("fixture size fits u64");
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
    let image = fs::read_dir(&appimages)
        .expect("AppImages exists")
        .flatten()
        .map(|entry| entry.path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "appimage")
        })
        .expect("one integrated image");

    // Make the installed file differ from the served one so the checker sees
    // an update without needing a second fixture.
    let mut installed_file = fs::OpenOptions::new()
        .append(true)
        .open(&image)
        .expect("open the integrated image");
    installed_file.write_all(b"x").expect("append one byte");
    drop(installed_file);

    let (url, stop, server) = serve(bytes);
    let set_args = [
        "--set-update-source".to_owned(),
        image.to_string_lossy().into_owned(),
        "--manager".to_owned(),
        "StaticFileUpdater".to_owned(),
        format!("url={url}"),
    ];
    let set_refs: Vec<&str> = set_args.iter().map(String::as_str).collect();
    let set = run(directory.path(), &set_refs);
    assert!(
        set.status.success(),
        "setting the source failed: {}",
        String::from_utf8_lossy(&set.stderr)
    );

    let updated = run(directory.path(), &["--update", "--all", "-y"]);
    assert!(
        updated.status.success(),
        "update failed: {}",
        String::from_utf8_lossy(&updated.stderr)
    );
    assert!(
        String::from_utf8_lossy(&updated.stdout).contains("updated successfully"),
        "stdout: {}",
        String::from_utf8_lossy(&updated.stdout)
    );
    assert_eq!(
        fs::metadata(&image).expect("updated image").len(),
        expected_size
    );

    stop.store(true, Ordering::SeqCst);
    server.join().expect("test server joins");
}
