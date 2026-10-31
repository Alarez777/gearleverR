//! Metadata extraction from the filesystem embedded in an AppImage.

use std::fs;
use std::path::{Path, PathBuf};

use crate::desktop::DesktopFile;
use crate::error::{Error, Result};

use super::squashfs::SquashfsImage;

/// Icon extracted from an AppImage payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedIcon {
    /// Suggested file name, such as `bettermediainfo.png`.
    pub file_name: String,
    /// Raw image bytes.
    pub bytes: Vec<u8>,
}

/// Metadata embedded in an AppImage's desktop entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppMetadata {
    /// `Name` from the desktop entry, or the desktop file stem as a fallback.
    pub name: String,
    /// `Comment`, empty when missing.
    pub comment: String,
    /// `Exec` line exactly as shipped.
    pub exec: String,
    /// `TryExec` line, empty when missing.
    pub try_exec: String,
    /// `Icon` value as shipped (may be a name or a path).
    pub icon_name: Option<String>,
    /// `Terminal=true` flag.
    pub terminal: bool,
    /// `X-AppImage-Version`, when the application declares one.
    pub version: Option<String>,
    /// Desktop file path inside the image.
    pub desktop_file: Option<String>,
    /// Raw desktop file contents.
    pub desktop_contents: String,
    /// Icon bytes, when one could be found.
    pub icon: Option<ExtractedIcon>,
}

/// Reads the desktop entry and icon from an extracted AppDir.
///
/// This is used when the payload is not SquashFS and an external extractor had
/// to unpack it first.
///
/// # Errors
///
/// Returns [`Error::MissingDesktopEntry`] when the directory has no top-level
/// `.desktop` file, [`Error::DesktopEntryNotUtf8`] when it is not UTF-8, and
/// [`Error::Io`] when files cannot be read.
pub fn read_from_directory(root: &Path) -> Result<AppMetadata> {
    let desktop_path = find_desktop_in_directory(root)
        .ok_or_else(|| Error::MissingDesktopEntry(root.to_path_buf()))?;
    let contents = fs::read_to_string(&desktop_path)
        .map_err(|_| Error::DesktopEntryNotUtf8(desktop_path.clone()))?;
    let entry = DesktopFile::parse(&contents);
    let main = entry.main_section();

    let name = main
        .get("Name")
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| {
            desktop_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Unknown".to_owned());

    let icon_name = main.get("Icon").filter(|value| !value.is_empty()).cloned();
    let icon = match icon_name.as_deref() {
        Some(icon_name) => find_icon_in_directory(root, icon_name),
        None => None,
    };

    Ok(AppMetadata {
        name,
        comment: main.get("Comment").cloned().unwrap_or_default(),
        exec: main.get("Exec").cloned().unwrap_or_default(),
        try_exec: main.get("TryExec").cloned().unwrap_or_default(),
        icon_name,
        terminal: main
            .get("Terminal")
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        version: main
            .get("X-AppImage-Version")
            .filter(|value| !value.is_empty())
            .cloned(),
        desktop_file: Some(
            desktop_path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        desktop_contents: contents,
        icon,
    })
}

/// Reads the desktop entry and icon from a SquashFS payload.
///
/// # Errors
///
/// Returns [`Error::MissingDesktopEntry`] when the image has no top-level
/// `.desktop` file, [`Error::DesktopEntryNotUtf8`] when it is not UTF-8, and the
/// SquashFS/I/O errors from [`SquashfsImage`].
pub fn read(path: &Path, offset: u64) -> Result<AppMetadata> {
    let image = SquashfsImage::open(path, offset)?;
    let desktop_path =
        find_desktop(&image).ok_or_else(|| Error::MissingDesktopEntry(path.to_path_buf()))?;

    let bytes = image
        .read(&desktop_path)?
        .ok_or_else(|| Error::MissingDesktopEntry(path.to_path_buf()))?;

    let contents =
        String::from_utf8(bytes).map_err(|_| Error::DesktopEntryNotUtf8(desktop_path.clone()))?;

    let entry = DesktopFile::parse(&contents);
    let main = entry.main_section();

    let name = main
        .get("Name")
        .filter(|value| !value.is_empty())
        .cloned()
        .or_else(|| {
            desktop_path
                .file_stem()
                .map(|stem| stem.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Unknown".to_owned());

    let icon_name = main.get("Icon").filter(|value| !value.is_empty()).cloned();

    let icon = match icon_name.as_deref() {
        Some(icon_name) => find_icon(&image, icon_name)?,
        None => None,
    };

    Ok(AppMetadata {
        name,
        comment: main.get("Comment").cloned().unwrap_or_default(),
        exec: main.get("Exec").cloned().unwrap_or_default(),
        try_exec: main.get("TryExec").cloned().unwrap_or_default(),
        icon_name,
        terminal: main
            .get("Terminal")
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
        version: main
            .get("X-AppImage-Version")
            .filter(|value| !value.is_empty())
            .cloned(),
        desktop_file: Some(desktop_path.to_string_lossy().into_owned()),
        desktop_contents: contents,
        icon,
    })
}

/// Finds a top-level desktop entry in an extracted directory.
fn find_desktop_in_directory(root: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = fs::read_dir(root)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"))
        })
        .collect();

    candidates.sort();
    candidates.into_iter().next()
}

/// Looks for an icon inside an extracted directory.
fn find_icon_in_directory(root: &Path, icon_name: &str) -> Option<ExtractedIcon> {
    if let Some(icon) = read_dir_icon_from_disk(root) {
        return Some(icon);
    }

    let base = strip_extension(icon_name);

    for candidate in icon_candidates(&base) {
        let path = root.join(&candidate);
        let Ok(bytes) = fs::read(&path) else {
            continue;
        };

        if let Some(extension) = image_extension(&bytes) {
            return Some(ExtractedIcon {
                file_name: format!("{base}{extension}"),
                bytes,
            });
        }
    }

    None
}

fn read_dir_icon_from_disk(root: &Path) -> Option<ExtractedIcon> {
    let path = root.join(".DirIcon");
    let bytes = fs::read(&path).ok()?;

    if let Some(extension) = image_extension(&bytes) {
        return Some(ExtractedIcon {
            file_name: format!("diricon{extension}"),
            bytes,
        });
    }

    let text = std::str::from_utf8(&bytes).ok()?.trim().to_owned();
    if text.is_empty() || text.contains('\n') {
        return None;
    }

    let target = root.join(text.trim_start_matches('/'));
    let bytes = fs::read(target).ok()?;

    image_extension(&bytes).map(|extension| ExtractedIcon {
        file_name: format!("diricon{extension}"),
        bytes,
    })
}

/// Finds the top-level desktop entry, preferring a deterministic order.
fn find_desktop(image: &SquashfsImage) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = image
        .paths()
        .filter(|path| path.parent() == Some(Path::new("/")))
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"))
        })
        .map(Path::to_path_buf)
        .collect();

    candidates.sort();
    candidates.into_iter().next()
}

/// Looks for the icon following the AppImage specification order.
fn find_icon(image: &SquashfsImage, icon_name: &str) -> Result<Option<ExtractedIcon>> {
    if let Some(icon) = read_dir_icon(image)? {
        return Ok(Some(icon));
    }

    let base = strip_extension(icon_name);

    for candidate in icon_candidates(&base) {
        let Some(bytes) = image.read(Path::new(&candidate))? else {
            continue;
        };

        if let Some(extension) = image_extension(&bytes) {
            return Ok(Some(ExtractedIcon {
                file_name: format!("{base}{extension}"),
                bytes,
            }));
        }
    }

    Ok(None)
}

/// Reads `.DirIcon`, which may be an image, a symlink or a path in a text file.
fn read_dir_icon(image: &SquashfsImage) -> Result<Option<ExtractedIcon>> {
    let path = Path::new(".DirIcon");
    let Some(bytes) = image.read(path)? else {
        return Ok(None);
    };

    if let Some(extension) = image_extension(&bytes) {
        return Ok(Some(ExtractedIcon {
            file_name: format!("diricon{extension}"),
            bytes,
        }));
    }

    let Ok(text) = std::str::from_utf8(&bytes) else {
        return Ok(None);
    };

    let target = text.trim();
    if target.is_empty() || target.contains('\n') {
        return Ok(None);
    }

    let Some(bytes) = image.read(Path::new(target.trim_start_matches('/')))? else {
        return Ok(None);
    };

    Ok(image_extension(&bytes).map(|extension| ExtractedIcon {
        file_name: format!("diricon{extension}"),
        bytes,
    }))
}

/// Candidate icon paths, mirroring the order used by the original project.
///
/// The root icon is often a symlink to a small `hicolor` size, so the theme
/// copies are tried first. A root SVG still wins because it scales.
fn icon_candidates(base: &str) -> Vec<String> {
    let mut candidates = vec![format!("{base}.svg")];

    for size in ["scalable", "512x512", "256x256", "128x128", "96x96"] {
        candidates.push(format!("usr/share/icons/hicolor/{size}/apps/{base}.svg"));
        candidates.push(format!("usr/share/icons/hicolor/{size}/apps/{base}.png"));
    }

    candidates.push(format!("{base}.png"));
    candidates
}

/// Removes a trailing `.png` or `.svg`, case-insensitively.
fn strip_extension(icon_name: &str) -> String {
    let lower = icon_name.to_ascii_lowercase();

    for extension in [".png", ".svg"] {
        if lower.ends_with(extension) {
            return icon_name[..icon_name.len() - extension.len()].to_owned();
        }
    }

    icon_name.to_owned()
}

/// Detects the image format from its magic bytes.
fn image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some(".png");
    }

    let start = bytes
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(bytes.len());

    let text = &bytes[start..];
    if text.starts_with(b"<?xml") || text.starts_with(b"<svg") || text.starts_with(b"<!DOCTYPE svg")
    {
        return Some(".svg");
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_known_icon_extensions() {
        assert_eq!(strip_extension("app.png"), "app");
        assert_eq!(strip_extension("app.SVG"), "app");
        assert_eq!(strip_extension("app"), "app");
        assert_eq!(strip_extension("app.icon"), "app.icon");
    }

    #[test]
    fn reads_metadata_from_an_extracted_directory() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory.path().join("Example.desktop"),
            "[Desktop Entry]\nName=Example\nExec=AppRun %U\nIcon=example\nX-AppImage-Version=2.1\n",
        )
        .unwrap();
        std::fs::write(
            directory.path().join("example.png"),
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
        )
        .unwrap();

        let metadata = read_from_directory(directory.path()).unwrap();

        assert_eq!(metadata.name, "Example");
        assert_eq!(metadata.version.as_deref(), Some("2.1"));
        assert_eq!(
            metadata.icon.as_ref().map(|icon| icon.file_name.as_str()),
            Some("example.png")
        );
    }

    #[test]
    fn reports_a_missing_desktop_entry() {
        let directory = tempfile::tempdir().unwrap();

        assert!(matches!(
            read_from_directory(directory.path()),
            Err(Error::MissingDesktopEntry(_))
        ));
    }

    #[test]
    fn prefers_high_resolution_icons_over_root_pngs() {
        let candidates = icon_candidates("example");
        let position = |path: &str| {
            candidates
                .iter()
                .position(|candidate| candidate == path)
                .unwrap()
        };

        assert!(
            position("usr/share/icons/hicolor/512x512/apps/example.png") < position("example.png")
        );
        assert!(position("example.svg") < position("example.png"));
    }

    #[test]
    fn detects_icon_formats() {
        assert_eq!(
            image_extension(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some(".png")
        );
        assert_eq!(image_extension(b"  <?xml version=\"1.0\"?>"), Some(".svg"));
        assert_eq!(image_extension(b"\n<svg xmlns=\"...\">"), Some(".svg"));
        assert_eq!(image_extension(b"not an image"), None);
    }
}
