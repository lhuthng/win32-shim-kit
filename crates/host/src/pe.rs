//! PE32 reader for the guest image.
//!
//! Only what the rehost needs: the section table, the import directory and
//! virtual-address lookups. The analyzed build has no relocation directory, so
//! nothing here rebases or patches the image.
//!
//! Addresses in this module are absolute (image base added) unless the name says
//! `rva`, matching the rehost's plan, where every documented address such as the
//! entry point `0x004AE604` is a VA.

use std::fs;
use std::path::Path;

use crate::Result;

/// The image maps here and never moves; see the plan's memory model.
pub const IMAGE_BASE: u32 = 0x0040_0000;

const MACHINE_I386: u16 = 0x014c;
const PE32_MAGIC: u16 = 0x010b;
/// Section headers follow the signature (4 bytes), COFF header (20) and
/// optional header, all measured from the PE signature.
const COFF_HEADER: usize = 24;
const SECTION_HEADER_SIZE: usize = 40;
/// PE32 puts the data directories after the fixed part of the optional header.
const DATA_DIRECTORY_OFFSET: usize = 96;
const NUMBER_OF_DIRECTORIES_OFFSET: usize = 92;
const IMPORT_DIRECTORY: usize = 1;
/// A hint/name entry is two bytes of hint followed by the name.
const HINT_NAME_SKIP: u32 = 2;
const MAX_IMPORT_DESCRIPTORS: usize = 4096;
const MAX_THUNKS: usize = 4096;
const MAX_CSTRING: usize = 4096;

fn u16_at(bytes: &[u8], at: usize) -> Result<u16> {
    let slice = bytes
        .get(at..at + 2)
        .ok_or_else(|| format!("PE field at file offset {at:#x} is outside the executable"))?;
    Ok(u16::from_le_bytes(slice.try_into().unwrap()))
}

fn u32_at(bytes: &[u8], at: usize) -> Result<u32> {
    let slice = bytes
        .get(at..at + 4)
        .ok_or_else(|| format!("PE field at file offset {at:#x} is outside the executable"))?;
    Ok(u32::from_le_bytes(slice.try_into().unwrap()))
}

/// One entry of the section table.
///
/// Addresses are relative to the image base, exactly as the PE stores them; add
/// [`IMAGE_BASE`] for the guest address. `Image` and `GuestMemory` both take VAs,
/// so this is the one place to be careful about the distinction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    pub virtual_address: u32,
    pub virtual_size: u32,
    pub raw: usize,
    pub raw_size: usize,
    pub characteristics: u32,
}

impl Section {
    /// Bytes this section occupies in the image (as an RVA span). Windows maps
    /// the larger of the declared virtual size and the raw size, which matters
    /// here: `DSEG` declares `0xe0` virtual bytes over a full raw page.
    pub fn span(&self) -> u32 {
        self.virtual_size.max(self.raw_size as u32)
    }

    pub fn end(&self) -> u32 {
        self.virtual_address.saturating_add(self.span())
    }

    pub fn contains(&self, va: u32) -> bool {
        va >= self.virtual_address && va < self.end()
    }

    pub fn is_executable(&self) -> bool {
        self.characteristics & 0x2000_0000 != 0
    }

    pub fn is_writable(&self) -> bool {
        self.characteristics & 0x8000_0000 != 0
    }
}

/// One IAT slot: the DLL and function name, plus the address the game calls
/// through. The shim's dispatch table is built from these.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Import {
    pub dll: String,
    pub name: String,
    pub iat_va: u32,
}

/// A parsed image. `bytes` is the file as shipped; `memory` copies from it.
#[derive(Clone, Debug)]
pub struct Image {
    pub bytes: Vec<u8>,
    pub image_base: u32,
    /// Absolute VA of the entry point (`0x004AE604` for this build).
    pub entry_point: u32,
    pub size_of_image: u32,
    pub size_of_headers: u32,
    pub sections: Vec<Section>,
    pub imports: Vec<Import>,
}

impl Image {
    pub fn from_path(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).map_err(|error| format!("cannot read {}: {error}", path.display()))?;
        Self::parse(bytes)
    }

    pub fn parse(bytes: Vec<u8>) -> Result<Self> {
        if u16_at(&bytes, 0)? != 0x5a4d {
            return Err("not an MZ executable".into());
        }
        let pe = u32_at(&bytes, 0x3c)? as usize;
        if u32_at(&bytes, pe)? != 0x0000_4550 {
            return Err("DOS header does not point at a PE signature".into());
        }

        let machine = u16_at(&bytes, pe + 4)?;
        if machine != MACHINE_I386 {
            return Err(format!(
                "expected an i386 image, found machine {machine:#06x}"
            ));
        }
        let section_count = u16_at(&bytes, pe + 6)? as usize;
        let optional_size = u16_at(&bytes, pe + 20)? as usize;
        let optional = pe + COFF_HEADER;
        let magic = u16_at(&bytes, optional)?;
        if magic != PE32_MAGIC {
            return Err(format!(
                "expected a PE32 optional header, found magic {magic:#06x}"
            ));
        }

        let image_base = u32_at(&bytes, optional + 28)?;
        let entry_rva = u32_at(&bytes, optional + 16)?;
        let size_of_image = u32_at(&bytes, optional + 56)?;
        let size_of_headers = u32_at(&bytes, optional + 60)?;

        let mut sections = Vec::with_capacity(section_count);
        let table = optional + optional_size;
        for index in 0..section_count {
            let header = table + index * SECTION_HEADER_SIZE;
            let name = bytes
                .get(header..header + 8)
                .ok_or("section table extends outside the executable")?;
            let name = String::from_utf8_lossy(name).trim_end_matches('\0').to_string();
            sections.push(Section {
                name,
                virtual_size: u32_at(&bytes, header + 8)?,
                virtual_address: u32_at(&bytes, header + 12)?,
                raw_size: u32_at(&bytes, header + 16)? as usize,
                raw: u32_at(&bytes, header + 20)? as usize,
                characteristics: u32_at(&bytes, header + 36)?,
            });
        }

        let mut image = Self {
            bytes,
            image_base,
            entry_point: image_base.wrapping_add(entry_rva),
            size_of_image,
            size_of_headers,
            sections,
            imports: Vec::new(),
        };
        image.imports = image.parse_imports(optional)?;
        Ok(image)
    }

    fn parse_imports(&self, optional: usize) -> Result<Vec<Import>> {
        let directories = u32_at(&self.bytes, optional + NUMBER_OF_DIRECTORIES_OFFSET)?;
        if directories <= IMPORT_DIRECTORY as u32 {
            return Ok(Vec::new());
        }
        let directory = DATA_DIRECTORY_OFFSET + IMPORT_DIRECTORY * 8;
        let import_rva = u32_at(&self.bytes, optional + directory)?;
        if import_rva == 0 {
            return Ok(Vec::new());
        }
        let mut offset = self
            .rva_to_offset(import_rva)
            .ok_or("import directory is not inside any section")?;

        let mut imports = Vec::new();
        for _ in 0..MAX_IMPORT_DESCRIPTORS {
            let name_rva = u32_at(&self.bytes, offset + 12)?;
            if name_rva == 0 {
                break;
            }
            let lookup = u32_at(&self.bytes, offset)?;
            let iat_rva = u32_at(&self.bytes, offset + 16)?;
            let dll = self.read_cstr(name_rva)?;
            let thunks = if lookup != 0 { lookup } else { iat_rva };
            let mut thunk = self
                .rva_to_offset(thunks)
                .ok_or_else(|| format!("{dll} import table is not inside any section"))?;

            for index in 0..MAX_THUNKS {
                let entry = u32_at(&self.bytes, thunk)?;
                if entry == 0 {
                    break;
                }
                let name = if entry & 0x8000_0000 != 0 {
                    format!("#{}", entry & 0xffff)
                } else {
                    self.read_cstr(entry + HINT_NAME_SKIP)?
                };
                imports.push(Import {
                    dll: dll.clone(),
                    name,
                    iat_va: self.image_base.wrapping_add(iat_rva + index as u32 * 4),
                });
                thunk += 4;
            }
            offset += 20;
        }
        Ok(imports)
    }

    /// Maps an RVA to a file offset, headers included. Bytes that exist only as
    /// zero-filled virtual space have no file offset.
    pub fn rva_to_offset(&self, rva: u32) -> Option<usize> {
        if rva < self.size_of_headers {
            return (rva as usize).lt(&self.bytes.len()).then_some(rva as usize);
        }
        let section = self.section_at(rva)?;
        let within = rva - section.virtual_address;
        if within as usize >= section.raw_size {
            return None;
        }
        Some(section.raw + within as usize)
    }

    pub fn section_at(&self, rva: u32) -> Option<&Section> {
        self.sections.iter().find(|section| section.contains(rva))
    }

    /// Reads a NUL-terminated ASCII string at an RVA.
    pub fn read_cstr(&self, rva: u32) -> Result<String> {
        let offset = self
            .rva_to_offset(rva)
            .ok_or_else(|| format!("string RVA {rva:#x} is not inside the executable"))?;
        let end = self.bytes[offset..]
            .iter()
            .take(MAX_CSTRING)
            .position(|byte| *byte == 0)
            .ok_or_else(|| format!("string at RVA {rva:#x} is not terminated"))?;
        Ok(String::from_utf8_lossy(&self.bytes[offset..offset + end]).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_reference_image() {
        let Some(image) = crate::reference_image() else {
            return;
        };
        assert_eq!(image.image_base, IMAGE_BASE);
        assert_eq!(image.entry_point, 0x004a_e604);
        assert_eq!(image.size_of_image, 0x000d_6000);
        assert_eq!(image.size_of_headers, 0x1000);
        assert_eq!(
            image
                .sections
                .iter()
                .map(|section| section.name.as_str())
                .collect::<Vec<_>>(),
            [".text", ".rdata", ".data", "DSEG", "CSEG", ".rsrc"]
        );

        let text = &image.sections[0];
        assert!(text.is_executable() && !text.is_writable());
        assert_eq!(text.virtual_address, 0x1000);
        assert_eq!(text.raw, 0x1000);
        assert!(!image.sections[3].is_executable() && image.sections[3].is_writable());
    }

    #[test]
    fn enumerates_every_import_slot() {
        let Some(image) = crate::reference_image() else {
            return;
        };
        assert_eq!(image.imports.len(), 162);
        let mut dlls: Vec<&str> = image.imports.iter().map(|import| import.dll.as_str()).collect();
        dlls.sort_unstable();
        dlls.dedup();
        assert_eq!(dlls.len(), 11);
        assert!(dlls.contains(&"KERNEL32.dll"));
        assert!(dlls.contains(&"DDRAW.dll"));

        // Every slot sits in `.data`'s IAT, between 0x004b9000 and 0x004bb000,
        // which is what the shim's dispatch handler indexes by.
        assert!(image
            .imports
            .iter()
            .all(|import| (0x004b_9000..0x004b_b000).contains(&import.iat_va)));
        assert!(image
            .imports
            .iter()
            .any(|import| import.dll == "DDRAW.dll" && import.name == "DirectDrawCreate"));
    }

    #[test]
    fn resolves_virtual_addresses_to_file_offsets() {
        let Some(image) = crate::reference_image() else {
            return;
        };
        assert_eq!(image.rva_to_offset(0), Some(0));
        assert_eq!(image.rva_to_offset(0x1000), Some(0x1000));
        // CSEG is mapped at 0xd1000 but stored at 0xcc000, so the mapping is not
        // an identity and is worth asserting.
        assert_eq!(image.rva_to_offset(0x000d_1000), Some(0x000c_c000));
        // Past the raw size of DSEG's padding there is no file offset.
        assert_eq!(image.rva_to_offset(0x000d_6000), None);
    }

    #[test]
    fn reads_strings_through_the_section_map() {
        let Some(image) = crate::reference_image() else {
            return;
        };
        let offset = image
            .bytes
            .windows(8)
            .position(|window| window == b"Version ")
            .expect("version string is present");
        let section = image
            .sections
            .iter()
            .find(|section| (section.raw..section.raw + section.raw_size).contains(&offset))
            .expect("version string lies inside a section");
        let rva = section.virtual_address + (offset - section.raw) as u32;
        assert!(image.read_cstr(rva).expect("reads").starts_with("Version "));
    }

    #[test]
    fn rejects_foreign_images() {
        assert!(Image::parse(b"not an executable".to_vec()).is_err());
        let mut truncated = vec![0u8; 0x40];
        truncated[0] = b'M';
        truncated[1] = b'Z';
        truncated[0x3c..0x40].copy_from_slice(&0x40_u32.to_le_bytes());
        assert!(Image::parse(truncated).is_err());
    }
}
