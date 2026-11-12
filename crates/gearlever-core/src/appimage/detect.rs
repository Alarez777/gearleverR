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
#[non_exhaustive]
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
#[non_exhaustive]
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

    /// Returns the architecture of the running binary.
    ///
    /// Returns `None` for host architectures this build does not map yet, so
    /// callers can avoid warning based on a guess.
    #[must_use]
    pub const fn host() -> Option<Self> {
        if cfg!(target_arch = "x86_64") {
            Some(Self::X86_64)
        } else if cfg!(target_arch = "aarch64") {
            Some(Self::Aarch64)
        } else if cfg!(target_arch = "x86") {
            Some(Self::Other(0x03))
        } else if cfg!(target_arch = "arm") {
            Some(Self::Other(0x28))
        } else if cfg!(target_arch = "riscv64") {
            Some(Self::Other(0xf3))
        } else if cfg!(target_arch = "loongarch64") {
            Some(Self::Other(0x102))
        } else {
            None
        }
    }

    /// Returns whether an image built for this architecture can run on the host.
    ///
    /// Unknown host architectures are treated as compatible, so callers only
    /// warn when this returns `false`.
    #[must_use]
    pub fn is_compatible_with_host(self) -> bool {
        Self::host().is_none_or(|host| host == self)
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
#[non_exhaustive]
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

impl PayloadKind {
    /// Returns the short name shown in the interface.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Squashfs => "SquashFS",
            Self::Dwarfs => "DwarFS",
            Self::Iso9660 => "ISO9660",
            Self::Unknown => "Unknown",
        }
    }
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

/// Container facts read from an AppImage in one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Inspection {
    pub(crate) image_type: ImageType,
    pub(crate) payload_offset: u64,
    pub(crate) payload_kind: PayloadKind,
    pub(crate) arch: ElfArch,
}

/// Reads the runtime header and the payload magic with a single file open.
pub(crate) fn inspect(path: &Path) -> Result<Inspection> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64];
    let read = file.read(&mut buffer)?;
    let magic = if read >= MAGIC_OFFSET + 3 {
        [
            buffer[MAGIC_OFFSET],
            buffer[MAGIC_OFFSET + 1],
            buffer[MAGIC_OFFSET + 2],
        ]
    } else {
        [0, 0, 0]
    };

    let Some(image_type) = image_type_from_magic(magic) else {
        return Err(Error::NotAnAppImage {
            path: path.to_path_buf(),
            magic,
        });
    };

    if read < 4 || buffer[0..4] != [0x7f, b'E', b'L', b'F'] {
        return Err(Error::InvalidElf("missing ELF magic"));
    }
    if read < 64 {
        return Err(Error::InvalidElf("truncated ELF header"));
    }

    let header = elf::header_from_bytes(&buffer)?;
    let payload_offset = header.payload_offset()?;
    let payload_kind = payload_kind_from_file(&mut file, payload_offset)?;

    Ok(Inspection {
        image_type,
        payload_offset,
        payload_kind,
        arch: ElfArch::from_machine(header.machine()),
    })
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

    match image_type_from_magic(magic) {
        Some(kind) => Ok(kind),
        None => Err(Error::NotAnAppImage {
            path: path.to_path_buf(),
            magic,
        }),
    }
}

/// Maps the runtime magic to an AppImage revision.
fn image_type_from_magic(magic: [u8; 3]) -> Option<ImageType> {
    match magic {
        [0x41, 0x49, 0x01] => Some(ImageType::Type1),
        [0x41, 0x49, 0x02] => Some(ImageType::Type2),
        _ => None,
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
    payload_kind_from_file(&mut file, offset)
}

fn payload_kind_from_file(file: &mut File, offset: u64) -> Result<PayloadKind> {
    file.seek(SeekFrom::Start(offset))?;

    let mut magic = [0_u8; 8];
    let read = file.read(&mut magic)?;

    if magic.starts_with(b"hsqs") {
        return Ok(PayloadKind::Squashfs);
    }

    if read >= 6 && magic.starts_with(b"DWARFS") {
        return Ok(PayloadKind::Dwarfs);
    }

    if is_iso9660(file, offset)? {
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

    #[test]
    fn payload_kind_labels_are_stable() {
        assert_eq!(PayloadKind::Squashfs.label(), "SquashFS");
        assert_eq!(PayloadKind::Dwarfs.label(), "DwarFS");
        assert_eq!(PayloadKind::Iso9660.label(), "ISO9660");
        assert_eq!(PayloadKind::Unknown.label(), "Unknown");
    }

    #[test]
    fn inspects_the_container_in_one_pass() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("App.AppImage");

        let mut image = vec![0_u8; 64];
        image[0..4].copy_from_slice(b"\x7fELF");
        image[4] = 2; // 64-bit ELF
        image[5] = 1; // little-endian
        image[8..11].copy_from_slice(b"AI\x02");
        image[0x12..0x14].copy_from_slice(&0x3e_u16.to_le_bytes()); // x86-64
        image[0x28..0x30].copy_from_slice(&64_u64.to_le_bytes()); // section table offset
        image[0x3a..0x3c].copy_from_slice(&64_u16.to_le_bytes()); // section entry size
        image.extend_from_slice(b"hsqs");
        image.extend_from_slice(&[0_u8; 4]);
        std::fs::write(&path, image).expect("write the fixture");

        let inspection = inspect(&path).expect("inspect the fixture");

        assert_eq!(inspection.image_type, ImageType::Type2);
        assert_eq!(inspection.payload_offset, 64);
        assert_eq!(inspection.payload_kind, PayloadKind::Squashfs);
        assert_eq!(inspection.arch, ElfArch::X86_64);
    }

    #[test]
    fn knows_the_host_architecture() {
        let host = ElfArch::host();

        if cfg!(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "arm",
            target_arch = "riscv64",
            target_arch = "loongarch64"
        )) {
            assert!(host.is_some(), "the build target must map to an ELF arch");
        }

        if let Some(host) = host {
            assert!(host.is_compatible_with_host());
        }
    }

    #[test]
    fn detects_a_foreign_architecture() {
        let Some(host) = ElfArch::host() else {
            return;
        };

        let foreign = if host == ElfArch::X86_64 {
            ElfArch::Aarch64
        } else {
            ElfArch::X86_64
        };

        assert!(!foreign.is_compatible_with_host());
        assert!(!ElfArch::Other(0xdead).is_compatible_with_host());
    }

    #[test]
    fn reads_the_machine_from_an_elf_header() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("aarch64.AppImage");

        let mut header = [0_u8; 20];
        header[0..4].copy_from_slice(b"\x7fELF");
        header[4] = 2; // 64-bit ELF
        header[5] = 1; // little-endian
        header[0x12..0x14].copy_from_slice(&0xb7_u16.to_le_bytes());

        std::fs::write(&path, header).expect("write the ELF header");

        assert_eq!(
            elf_arch(&path).expect("read the architecture"),
            ElfArch::Aarch64
        );
    }
}
