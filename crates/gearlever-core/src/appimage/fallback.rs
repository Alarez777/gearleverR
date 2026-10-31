//! External extractors for payloads that are not SquashFS.
//!
//! DwarFS and ISO9660 images cannot be read in-process yet. When the matching
//! tool is installed, Gear LeverR extracts only the desktop entry and icons
//! into a temporary directory. Running the AppImage itself is a last resort and
//! is gated by the `block-unsafe-extractor` setting.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::error::{Error, Result};

use super::{PayloadKind, metadata};

/// Extracts metadata with an external tool.
///
/// # Errors
///
/// Returns [`Error::MissingExtractor`] when no tool can handle the payload,
/// [`Error::Io`] on filesystem failures and the metadata errors from
/// [`metadata::read_from_directory`].
#[expect(
    clippy::fn_params_excessive_bools,
    reason = "the security switch mirrors the block-unsafe-extractor setting"
)]
pub fn read(
    image: &Path,
    kind: PayloadKind,
    work_dir: &Path,
    allow_unsafe_extractor: bool,
) -> Result<super::AppMetadata> {
    fs::create_dir_all(work_dir)?;

    let mut extracted = false;

    if kind == PayloadKind::Dwarfs {
        extracted = dwarfsextract(image, work_dir) && has_desktop(work_dir);
    }
    if !extracted {
        extracted = seven_zip(image, work_dir) && has_desktop(work_dir);
    }
    if !extracted && allow_unsafe_extractor {
        extracted = appimage_extract(image, work_dir)? && has_desktop(work_dir);
    }
    if !extracted {
        return Err(Error::MissingExtractor(image.to_path_buf()));
    }

    let root = if work_dir.join("squashfs-root").is_dir() {
        work_dir.join("squashfs-root")
    } else {
        work_dir.to_path_buf()
    };

    metadata::read_from_directory(&root)
}

fn dwarfsextract(image: &Path, work_dir: &Path) -> bool {
    run(
        "dwarfsextract",
        &[
            format!("--input={}", image.display()),
            format!("--output={}", work_dir.display()),
            "--pattern=**.png".to_owned(),
            "--pattern=**.svg".to_owned(),
            "--pattern=**.desktop".to_owned(),
            "--pattern=.DirIcon".to_owned(),
        ],
    )
}

fn seven_zip(image: &Path, work_dir: &Path) -> bool {
    let output = format!("-o{}", work_dir.display());
    let arguments = [
        "x",
        &image.to_string_lossy(),
        &output,
        "-y",
        "-bso0",
        "-bsp0",
        "-ir!*.desktop",
        "-ir!*.png",
        "-ir!*.svg",
        "-ir0!.DirIcon",
    ];

    run("7zz", &arguments)
}

fn has_desktop(work_dir: &Path) -> bool {
    let root = if work_dir.join("squashfs-root").is_dir() {
        work_dir.join("squashfs-root")
    } else {
        work_dir.to_path_buf()
    };

    std::fs::read_dir(root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("desktop"))
        })
    })
}

fn appimage_extract(image: &Path, work_dir: &Path) -> Result<bool> {
    let copy: PathBuf = work_dir.join("app.AppImage");
    fs::copy(image, &copy)?;

    let mut permissions = fs::metadata(&copy)?.permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&copy, permissions)?;

    let status = Command::new(&copy)
        .arg("--appimage-extract")
        .current_dir(work_dir)
        .status();

    Ok(status.is_ok_and(|status| status.success()))
}

fn run(tool: &str, arguments: &[impl AsRef<std::ffi::OsStr>]) -> bool {
    match Command::new(tool).args(arguments).status() {
        Ok(status) => status.success(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(tool, "extractor not installed");
            false
        }
        Err(error) => {
            tracing::warn!(tool, %error, "extractor failed to start");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_a_missing_extractor() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("image.AppImage");
        fs::write(&image, b"not really an image").unwrap();

        let result = read(
            &image,
            PayloadKind::Dwarfs,
            &directory.path().join("work"),
            false,
        );

        assert!(matches!(result, Err(Error::MissingExtractor(_))));
    }
}
