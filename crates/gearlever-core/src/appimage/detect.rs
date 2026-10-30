//! Detection of AppImage containers: revision, payload offset, architecture.

use std::fmt;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{Error, Result};

use super::elf;

const MAGIC_OFFSET: usize = 8;

/// AppImage format revision, taken from the runtime magic at offset 8.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageType {
    /// Type 1: the payload is an ISO9660 image.
    Type1,
    /// Type 2: the payload is a SquashFS (or DwarFS) image.
    Type2,
}

impl ImageType {
    /// Returns the byte that encodes this revision in the runtime magic.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Type1 => 1,
            Self::Type2 => 2,
        }
    }
}

impl fmt::Display for ImageType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Type1 => f.write_str("type 1"),
            Self::Type2 => f.write_str("type 2"),
        }
    }
}

/// CPU architecture encoded in the ELF header of the AppImage runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ElfArch {
    /// x86-64 (`EM_X86_64`).
    X86_64,
    /// AArch64 (`EM_AARCH64`).
    Aarch64,
    /// Any other machine type, preserved as its ELF code.
    Other(u16),
}

impl ElfArch {
    /// Maps an ELF `e_machine` value to an architecture.
    #[must_use]
    pub const fn from_machine(machine: u16) -> Self {
        match machine {
            0x3e => Self::X86_64,
            0xb7 => Self::Aarch64,
            other => Self::Other(other),
        }
    }

    /// Returns a short human-readable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Other(_) => "unknown",
        }
    }
}

impl fmt::Display for ElfArch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Other(machine) => write!(f, "unknown (e_machine 0x{machine:04x})"),
            other => f.write_str(other.name()),
        }
    }
}

/// Filesystem found right after the ELF runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayloadKind {
    /// SquashFS, readable in-process by `backhand`.
    Squashfs,
    /// DwarFS, which needs the external `dwarfsextract` tool for now.
    Dwarfs,
    /// ISO9660 (type 1 images).
    Iso9660,
    /// Recognized AppImage whose payload format is not known.
    Unknown,
}

impl fmt::Display for PayloadKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Squashfs => f.write_str("squashfs"),
            Self::Dwarfs => f.write_str("dwarfs"),
            Self::Iso9660 => f.write_str("iso9660"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

/// Reads the AppImage revision from the runtime magic.
///
/// # Errors
///
/// Returns [`Error::NotAnAppImage`] when the file is shorter than the magic or
/// does not contain an AppImage runtime, and [`Error::Io`] on read failure.
pub fn image_type(path: &Path) -> Result<ImageType> {
    let mut file = File::open(path)?;
    let mut header = [0_u8; MAGIC_OFFSET + 3];
    let read = file.read(&mut header)?;
    let magic = if read >= MAGIC_OFFSET + 3 {
        [
            header[MAGIC_OFFSET],
            header[MAGIC_OFFSET + 1],
            header[MAGIC_OFFSET + 2],
        ]
    } else {
        [0, 0, 0]
    };

    match magic {
        [0x41, 0x49, 0x01] => Ok(ImageType::Type1),
        [0x41, 0x49, 0x02] => Ok(ImageType::Type2),
        _ => Err(Error::NotAnAppImage {
            path: path.to_path_buf(),
            magic,
        }),
    }
}

/// Returns the offset where the embedded payload starts.
///
/// AppImages append their payload after the ELF runtime; the end of the section
/// header table is where it begins. This matches `get_appimage_offset.sh` from
/// the original project.
///
/// # Errors
///
/// Returns [`Error::InvalidElf`] if the runtime header is truncated or uses an
/// unknown class/encoding, and [`Error::Io`] on read failure.
pub fn payload_offset(path: &Path) -> Result<u64> {
    elf::header(path)?.payload_offset()
}

/// Reads the architecture from the AppImage runtime ELF header.
///
/// # Errors
///
/// Returns [`Error::InvalidElf`] if the header is truncated or not ELF, and
/// [`Error::Io`] on read failure.
pub fn elf_arch(path: &Path) -> Result<ElfArch> {
    Ok(ElfArch::from_machine(elf::machine(path)?))
}

/// Sniffs the filesystem stored at `offset`.
///
/// # Errors
///
/// Returns [`Error::Io`] when the file cannot be read.
pub fn payload_kind(path: &Path, offset: u64) -> Result<PayloadKind> {
    let mut file = File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;

    let mut magic = [0_u8; 8];
    let read = file.read(&mut magic)?;

    if magic.starts_with(b"hsqs") {
        return Ok(PayloadKind::Squashfs);
    }

    if read >= 6 && magic.starts_with(b"DWARFS") {
        return Ok(PayloadKind::Dwarfs);
    }

    if is_iso9660(&mut file, offset)? {
        return Ok(PayloadKind::Iso9660);
    }

    Ok(PayloadKind::Unknown)
}

fn is_iso9660(file: &mut File, offset: u64) -> Result<bool> {
    // The primary volume descriptor starts at sector 16 and carries "CD001".
    let descriptor = offset
        .checked_add(0x8001)
        .ok_or(Error::InvalidElf("ISO9660 offset overflow"))?;
    file.seek(SeekFrom::Start(descriptor))?;

    let mut marker = [0_u8; 5];
    match file.read_exact(&mut marker) {
        Ok(()) => Ok(&marker == b"CD001"),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => Err(Error::Io(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_known_machine_types() {
        assert_eq!(ElfArch::from_machine(0x3e), ElfArch::X86_64);
        assert_eq!(ElfArch::from_machine(0xb7), ElfArch::Aarch64);
        assert_eq!(ElfArch::from_machine(0x03), ElfArch::Other(0x03));
    }

    #[test]
    fn image_type_names_are_stable() {
        assert_eq!(ImageType::Type1.to_string(), "type 1");
        assert_eq!(ImageType::Type2.to_string(), "type 2");
    }
}
