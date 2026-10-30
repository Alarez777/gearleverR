//! AppImage container handling.

pub mod detect;
pub mod metadata;
pub mod squashfs;

pub use detect::{ElfArch, ImageType, PayloadKind};
pub use metadata::{AppMetadata, ExtractedIcon};
pub use squashfs::SquashfsImage;

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

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
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedPayload`] for payloads that are not SquashFS
    /// yet, plus the errors documented on [`metadata::read`].
    pub fn metadata(&self) -> Result<AppMetadata> {
        match self.payload_kind {
            PayloadKind::Squashfs => metadata::read(&self.path, self.payload_offset),
            _ => Err(Error::UnsupportedPayload(self.path.clone())),
        }
    }
}
