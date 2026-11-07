//! Shared fixtures for update-manager tests.

use std::io::Cursor;

use zsync_rs::ControlFile;

use super::test_server::{Route, TestServer};

pub(crate) const BLOCK_SIZE: usize = 1024;
pub(crate) const FILE_SIZE: usize = 64 * 1024;

/// Deterministic bytes that are not a run of a single value, so repeated
/// blocks do not hide mistakes in the matcher.
pub(crate) fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect()
}

/// Returns a copy of `old` with four separate blocks modified.
pub(crate) fn changed_copy(old: &[u8]) -> Vec<u8> {
    let mut new = old.to_vec();
    for offset in [8 * 1024, 24 * 1024, 40 * 1024, 56 * 1024] {
        new[offset] = new[offset].wrapping_add(1);
    }
    new
}

/// Generates the `.zsync` control file for `new`.
pub(crate) fn control_bytes(new: &[u8], url: &str) -> Vec<u8> {
    let mut reader = Cursor::new(new);
    let control =
        ControlFile::generate(&mut reader, "new.AppImage", url, Some(BLOCK_SIZE)).unwrap();
    let mut bytes = Vec::new();
    control.write(&mut bytes).unwrap();
    bytes
}

/// Serves the control file and `target` on the paths the tests use.
pub(crate) fn delta_server(control: Vec<u8>, target: Route) -> TestServer {
    TestServer::start(vec![
        ("/new.AppImage.zsync", Route::new(control)),
        ("/new.AppImage", target),
    ])
}
