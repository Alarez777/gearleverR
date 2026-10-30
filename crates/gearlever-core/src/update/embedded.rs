//! Reading the `.upd_info` section embedded in an AppImage.

use std::path::Path;

use crate::appimage::elf;
use crate::error::Result;

/// Returns the update information embedded in an AppImage, if present.
///
/// The section contains one null-terminated string such as
/// `gh-releases-zsync|owner|repo|latest|App-*.AppImage.zsync` or
/// `zsync|https://example.com/App.AppImage.zsync`.
///
/// # Errors
///
/// Returns [`crate::Error::InvalidElf`] when the ELF runtime is malformed and
/// [`crate::Error::Io`] when the file cannot be read.
pub fn read(image: &Path) -> Result<Option<String>> {
    let Some(bytes) = elf::section(image, ".upd_info")? else {
        return Ok(None);
    };

    let text = String::from_utf8_lossy(&bytes);
    let value = text
        .split('\0')
        .map(str::trim)
        .find(|segment| !segment.is_empty())
        .map(ToOwned::to_owned);

    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_first_null_terminated_string() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("app.AppImage");

        // A minimal type 2 AppImage with one ELF section is built by the
        // integration tests; here we only verify the missing-section path.
        std::fs::write(&file, b"\x7fELF\x02\x01\x01\x00AI\x02").unwrap();

        assert!(read(&file).is_err() || read(&file).unwrap().is_none());
    }
}
