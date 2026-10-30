//! Integration tests that read real AppImages.
//!
//! The fixtures are not committed, so the tests are opt-in:
//!
//! ```sh
//! GEARLEVER_TEST_APPIMAGE=~/AppImages/app.appimage cargo test -p gearlever-core
//! ```

use std::path::PathBuf;

use gearlever_core::appimage::{AppImage, ImageType, PayloadKind};

fn test_appimage() -> Option<PathBuf> {
    std::env::var_os("GEARLEVER_TEST_APPIMAGE").map(PathBuf::from)
}

#[test]
fn detects_container_and_metadata() {
    let Some(path) = test_appimage() else {
        return;
    };

    let appimage = AppImage::open(&path).expect("the fixture must be a valid AppImage");

    assert_eq!(appimage.image_type(), ImageType::Type2);
    assert!(appimage.payload_offset() > 0);
    assert_eq!(appimage.payload_kind(), PayloadKind::Squashfs);
    assert_ne!(appimage.arch().name(), "unknown");

    let metadata = appimage.metadata().expect("metadata must be readable");
    assert!(!metadata.name.is_empty(), "desktop entry has a name");
    assert!(
        metadata.desktop_file.is_some(),
        "desktop entry path is reported"
    );
    assert!(
        metadata.icon.is_some(),
        "fixture should ship an icon in the image root"
    );
}

#[test]
fn rejects_files_that_are_not_appimages() {
    let error = AppImage::open("/etc/hostname").expect_err("a text file is not an AppImage");
    assert!(matches!(error, gearlever_core::Error::NotAnAppImage { .. }));
}
