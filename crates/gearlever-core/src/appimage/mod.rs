//! AppImage container handling.

pub mod detect;
pub(crate) mod elf;
pub mod fallback;
pub mod metadata;
pub mod running;
pub mod squashfs;

pub use detect::{ElfArch, ImageType, PayloadKind};
pub use fallback::read as read_with_extractor;
pub use metadata::{AppMetadata, ExtractedIcon};
pub use running::is_running;
pub use squashfs::SquashfsImage;

use std::path::{Path, PathBuf};

use crate::error::Result;

/// Options for reading metadata from payloads that need external tools.
#[derive(Debug, Clone)]
pub struct MetadataOptions {
    /// Whether running the AppImage with `--appimage-extract` is allowed.
    pub allow_unsafe_extractor: bool,
    /// Directory where temporary extractions are created.
    pub work_dir: PathBuf,
}

impl Default for MetadataOptions {
    fn default() -> Self {
        Self {
            allow_unsafe_extractor: false,
            work_dir: std::env::temp_dir().join("gearlever-r-metadata"),
        }
    }
}

/// An AppImage file opened for inspection.
///
/// Opening reads only the ELF runtime and the payload superblock; the embedded
/// filesystem is parsed lazily when [`AppImage::metadata`] is called.
#[derive(Debug, Clone)]
pub struct AppImage {
    path: PathBuf,
    image_type: ImageType,
    payload_offset: u64,
    payload_kind: PayloadKind,
    arch: ElfArch,
}

impl AppImage {
    /// Opens an AppImage and detects its revision, payload and architecture.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NotAnAppImage`] for other file types, [`Error::InvalidElf`]
    /// for a corrupt runtime, and [`Error::Io`] when the file cannot be read.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let image_type = detect::image_type(&path)?;
        let payload_offset = detect::payload_offset(&path)?;
        let payload_kind = detect::payload_kind(&path, payload_offset)?;
        let arch = detect::elf_arch(&path)?;

        Ok(Self {
            path,
            image_type,
            payload_offset,
            payload_kind,
            arch,
        })
    }

    /// Returns the path this image was opened from.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the AppImage revision.
    #[must_use]
    pub const fn image_type(&self) -> ImageType {
        self.image_type
    }

    /// Returns the offset where the embedded payload starts.
    #[must_use]
    pub const fn payload_offset(&self) -> u64 {
        self.payload_offset
    }

    /// Returns the embedded filesystem format.
    #[must_use]
    pub const fn payload_kind(&self) -> PayloadKind {
        self.payload_kind
    }

    /// Returns the architecture declared by the runtime.
    #[must_use]
    pub const fn arch(&self) -> ElfArch {
        self.arch
    }

    /// Reads the desktop entry and icon embedded in the AppImage.
    ///
    /// Payloads that are not SquashFS return [`Error::UnsupportedPayload`]; use
    /// [`AppImage::metadata_with`] to let an external extractor handle them.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedPayload`] for non-SquashFS payloads, plus the
    /// errors documented on [`metadata::read`].
    pub fn metadata(&self) -> Result<AppMetadata> {
        self.metadata_with(&MetadataOptions::default())
    }

    /// Reads the desktop entry and icon, using external tools when needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingExtractor`] when a DwarFS or ISO9660 payload
    /// needs a tool that is not installed, plus the errors documented on
    /// [`metadata::read`] and [`metadata::read_from_directory`].
    pub fn metadata_with(&self, options: &MetadataOptions) -> Result<AppMetadata> {
        match self.payload_kind {
            PayloadKind::Squashfs => metadata::read(&self.path, self.payload_offset),
            other => {
                let nanos = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos();
                let work_dir = options.work_dir.join(format!("extract-{nanos}"));
                let result =
                    fallback::read(&self.path, other, &work_dir, options.allow_unsafe_extractor);
                let _ = std::fs::remove_dir_all(&work_dir);
                result
            }
        }
    }
}
