//! Minimal ELF reader for the AppImage runtime.
//!
//! AppImages are ELF executables with a payload appended after the section
//! header table. Reading the runtime directly lets Gear LeverR find that
//! offset, the architecture and string sections such as `.upd_info` without
//! spawning `readelf`, `strings` or `file`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use crate::error::{Error, Result};

const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];

/// Upper bound on section headers, to reject corrupt or hostile files.
const MAX_SECTIONS: u16 = 4096;

/// Byte order declared by the ELF header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Endian {
    Little,
    Big,
}

impl Endian {
    fn from_elf(encoding: u8) -> Result<Self> {
        match encoding {
            1 => Ok(Self::Little),
            2 => Ok(Self::Big),
            _ => Err(Error::InvalidElf("unknown ELF data encoding")),
        }
    }
}

/// The parts of the ELF header needed to read sections.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ElfHeader {
    /// 64-bit ELF (`EI_CLASS == 2`) versus 32-bit.
    is_64: bool,
    endian: Endian,
    machine: u16,
    section_offset: u64,
    section_entry_size: u16,
    section_count: u16,
    string_table_index: u16,
}

impl ElfHeader {
    /// Returns the machine type declared in the header.
    pub(crate) const fn machine(&self) -> u16 {
        self.machine
    }

    /// Returns the offset where the appended payload starts.
    pub(crate) fn payload_offset(&self) -> Result<u64> {
        let table_size = u64::from(self.section_entry_size)
            .checked_mul(u64::from(self.section_count))
            .ok_or(Error::InvalidElf("section table size overflow"))?;

        self.section_offset
            .checked_add(table_size)
            .ok_or(Error::InvalidElf("payload offset overflow"))
    }

    /// Reads a named section from the file.
    pub(crate) fn section(&self, path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
        let mut file = File::open(path)?;
        let mut headers = Vec::with_capacity(usize::from(self.section_count));

        for index in 0..u64::from(self.section_count) {
            let offset = self
                .section_offset
                .checked_add(index * u64::from(self.section_entry_size))
                .ok_or(Error::InvalidElf("section header offset overflow"))?;
            file.seek(SeekFrom::Start(offset))?;

            let mut buffer = [0_u8; 64];
            file.read_exact(&mut buffer[..usize::from(self.section_entry_size).min(64)])?;
            headers.push(if self.is_64 {
                SectionHeader::parse_64(&buffer, self.endian)?
            } else {
                SectionHeader::parse_32(&buffer, self.endian)?
            });
        }

        let Some(strings) = headers.get(usize::from(self.string_table_index)) else {
            return Ok(None);
        };
        let names = strings.read_data(&mut file)?;

        for header in &headers {
            let Some(section_name) = read_c_string(&names, header.name_offset) else {
                continue;
            };
            if section_name == name {
                return header.read_data(&mut file).map(Some);
            }
        }

        Ok(None)
    }
}

/// Opens a file and parses its ELF header.
pub(crate) fn header(path: &Path) -> Result<ElfHeader> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 64];
    let read = file.read(&mut buffer)?;

    if read < 4 || buffer[0..4] != ELF_MAGIC {
        return Err(Error::InvalidElf("missing ELF magic"));
    }
    if read < 64 {
        return Err(Error::InvalidElf("truncated ELF header"));
    }

    header_from_bytes(&buffer)
}

/// Parses an ELF header already read from a file.
pub(crate) fn header_from_bytes(buffer: &[u8; 64]) -> Result<ElfHeader> {
    if buffer[0..4] != ELF_MAGIC {
        return Err(Error::InvalidElf("missing ELF magic"));
    }

    let is_64 = match buffer[4] {
        2 => true,
        1 => false,
        _ => return Err(Error::InvalidElf("unknown ELF class")),
    };
    let endian = Endian::from_elf(buffer[5])?;
    let machine = u16_at(buffer, 0x12, endian)?;

    let (section_offset, section_entry_size, section_count, string_table_index) = if is_64 {
        (
            u64_at(buffer, 0x28, endian)?,
            u16_at(buffer, 0x3a, endian)?,
            u16_at(buffer, 0x3c, endian)?,
            u16_at(buffer, 0x3e, endian)?,
        )
    } else {
        (
            u64::from(u32_at(buffer, 0x20, endian)?),
            u16_at(buffer, 0x2e, endian)?,
            u16_at(buffer, 0x30, endian)?,
            u16_at(buffer, 0x32, endian)?,
        )
    };

    if section_count > MAX_SECTIONS {
        return Err(Error::InvalidElf("too many section headers"));
    }

    Ok(ElfHeader {
        is_64,
        endian,
        machine,
        section_offset,
        section_entry_size,
        section_count,
        string_table_index,
    })
}

/// Reads a named section from an ELF file.
pub(crate) fn section(path: &Path, name: &str) -> Result<Option<Vec<u8>>> {
    header(path)?.section(path, name)
}

/// Parses the machine type from the ELF header.
pub(crate) fn machine(path: &Path) -> Result<u16> {
    let mut file = File::open(path)?;
    let mut buffer = [0_u8; 20];
    file.read_exact(&mut buffer)?;

    if buffer[0..4] != ELF_MAGIC {
        return Err(Error::InvalidElf("missing ELF magic"));
    }

    let endian = Endian::from_elf(buffer[5])?;
    u16_at(&buffer, 0x12, endian)
}

#[derive(Debug, Clone, Copy)]
struct SectionHeader {
    name_offset: u32,
    offset: u64,
    size: u64,
}

impl SectionHeader {
    fn parse_64(buffer: &[u8; 64], endian: Endian) -> Result<Self> {
        Ok(Self {
            name_offset: u32_at(buffer, 0, endian)?,
            offset: u64_at(buffer, 24, endian)?,
            size: u64_at(buffer, 32, endian)?,
        })
    }

    fn parse_32(buffer: &[u8; 64], endian: Endian) -> Result<Self> {
        Ok(Self {
            name_offset: u32_at(buffer, 0, endian)?,
            offset: u64::from(u32_at(buffer, 16, endian)?),
            size: u64::from(u32_at(buffer, 20, endian)?),
        })
    }

    fn read_data(&self, file: &mut File) -> Result<Vec<u8>> {
        const MAX_SECTION_SIZE: u64 = 4 * 1024 * 1024;

        if self.size > MAX_SECTION_SIZE {
            return Err(Error::InvalidElf("section is too large"));
        }

        let mut data = vec![
            0_u8;
            usize::try_from(self.size)
                .map_err(|_| Error::InvalidElf("section is too large"))?
        ];
        file.seek(SeekFrom::Start(self.offset))?;
        file.read_exact(&mut data)?;

        Ok(data)
    }
}

fn read_c_string(data: &[u8], offset: u32) -> Option<&str> {
    let start = usize::try_from(offset).ok()?;
    let tail = data.get(start..)?;
    let end = tail.iter().position(|byte| *byte == 0)?;

    std::str::from_utf8(&tail[..end]).ok()
}

fn u16_at(buffer: &[u8], offset: usize, endian: Endian) -> Result<u16> {
    let bytes: [u8; 2] = buffer
        .get(offset..offset + 2)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(Error::InvalidElf("truncated ELF header"))?;

    Ok(match endian {
        Endian::Little => u16::from_le_bytes(bytes),
        Endian::Big => u16::from_be_bytes(bytes),
    })
}

fn u32_at(buffer: &[u8], offset: usize, endian: Endian) -> Result<u32> {
    let bytes: [u8; 4] = buffer
        .get(offset..offset + 4)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(Error::InvalidElf("truncated ELF header"))?;

    Ok(match endian {
        Endian::Little => u32::from_le_bytes(bytes),
        Endian::Big => u32::from_be_bytes(bytes),
    })
}

fn u64_at(buffer: &[u8], offset: usize, endian: Endian) -> Result<u64> {
    let bytes: [u8; 8] = buffer
        .get(offset..offset + 8)
        .and_then(|slice| slice.try_into().ok())
        .ok_or(Error::InvalidElf("truncated ELF header"))?;

    Ok(match endian {
        Endian::Little => u64::from_le_bytes(bytes),
        Endian::Big => u64::from_be_bytes(bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_offsets_with_both_endiannesses() {
        assert_eq!(u16_at(&[0x01, 0x02], 0, Endian::Little).unwrap(), 0x0201);
        assert_eq!(u16_at(&[0x01, 0x02], 0, Endian::Big).unwrap(), 0x0102);
    }

    #[test]
    fn rejects_missing_magic() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-elf");
        std::fs::write(&file, b"hello").unwrap();

        assert!(matches!(header(&file), Err(Error::InvalidElf(_))));
    }
}
