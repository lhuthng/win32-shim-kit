//! Guest linear memory: the image at its fixed base, plus the stack, TEB and
//! heap arena the Win32 layer expects.
//!
//! One flat buffer covers [`IMAGE_BASE`] to [`MEMORY_TOP`], so every access is a
//! single bounds check and giving the CPU a slice is trivial. Address zero is
//! deliberately outside the buffer, so a null-pointer bug faults instead of
//! silently reading the DOS header.
//!
//! Writes are checked against the PE section flags: `.text` is read-only and the
//! image headers are read-only, exactly as they are in the guest. This is the
//! cheapest possible `VirtualProtect` substitute, and the game never asks for
//! one -- `VirtualProtect` is not imported.

use crate::pe::{Image, Section, IMAGE_BASE};
use crate::Result;

/// Thread environment block, one page below the stack. `fs:[0]` points here.
pub const TEB_BASE: u32 = 0x00ff_f000;
pub const STACK_BASE: u32 = 0x0100_0000;
pub const STACK_SIZE: u32 = 0x0010_0000;
pub const STACK_TOP: u32 = STACK_BASE + STACK_SIZE;
/// Heap arena reserved until the CRT heap lands on top of it. A board session
/// allocates a surface per tile and a decoded map per screen, and 32 MB ran out
/// while the first board was loading, so the reserve is sized for a session
/// rather than for the boot.
pub const HEAP_BASE: u32 = STACK_TOP;
pub const HEAP_RESERVE: u32 = 0x0800_0000;
pub const MEMORY_TOP: u32 = HEAP_BASE + HEAP_RESERVE;

/// `NT_TIB`, which is what `fs:` refers to in this image.
const TIB_EXCEPTION_LIST: u32 = 0x00;
const TIB_STACK_BASE: u32 = 0x04;
const TIB_STACK_LIMIT: u32 = 0x08;
const TIB_SELF: u32 = 0x18;

const MAX_CSTRING: usize = 4096;

pub struct GuestMemory {
    store: Vec<u8>,
    sections: Vec<Section>,
    pub entry_point: u32,
    pub size_of_image: u32,
    pub imports: Vec<crate::pe::Import>,
}

impl GuestMemory {
    /// Maps the image and sets up the TEB. The image must want
    /// [`IMAGE_BASE`]; there is no relocation support by design.
    pub fn load(image: Image) -> Result<Self> {
        let Image {
            bytes: file,
            image_base,
            entry_point,
            size_of_image,
            size_of_headers,
            sections,
            imports,
        } = image;

        if image_base != IMAGE_BASE {
            return Err(format!(
                "image wants base {image_base:#010x}, but the guest is fixed at {IMAGE_BASE:#010x}"
            ));
        }
        if size_of_image > TEB_BASE - IMAGE_BASE {
            return Err(format!(
                "image maps {size_of_image:#x} bytes, which collides with the TEB at {TEB_BASE:#010x}"
            ));
        }
        for section in &sections {
            if section.end() > size_of_image {
                return Err(format!(
                    "{} section ends at {:#010x}, past the declared image size",
                    section.name,
                    section.end()
                ));
            }
            if section.raw + section.raw_size > file.len() {
                return Err(format!(
                    "{} section reads past the end of the executable",
                    section.name
                ));
            }
        }

        let mut store = vec![0_u8; (MEMORY_TOP - IMAGE_BASE) as usize];
        let headers = (size_of_headers as usize).min(file.len());
        store[..headers].copy_from_slice(&file[..headers]);
        for section in &sections {
            let start = section.virtual_address as usize;
            store[start..start + section.raw_size]
                .copy_from_slice(&file[section.raw..section.raw + section.raw_size]);
        }

        let mut memory = Self {
            store,
            sections,
            entry_point,
            size_of_image,
            imports,
        };
        memory.write_u32(TEB_BASE + TIB_EXCEPTION_LIST, u32::MAX)?;
        memory.write_u32(TEB_BASE + TIB_STACK_BASE, STACK_TOP)?;
        memory.write_u32(TEB_BASE + TIB_STACK_LIMIT, STACK_BASE)?;
        memory.write_u32(TEB_BASE + TIB_SELF, TEB_BASE)?;
        Ok(memory)
    }

    /// A blank, writable window at [`IMAGE_BASE`], for tests and for the
    /// throughput benchmark in `trace`. One read-write section covers the whole
    /// window, so every address in it can be written; it is not an image and
    /// cannot be introspected as one.
    pub fn empty(size: usize) -> Result<Self> {
        if size == 0 || size > (MEMORY_TOP - IMAGE_BASE) as usize {
            return Err(format!(
                "a blank memory of {size} bytes does not fit below {MEMORY_TOP:#010x}"
            ));
        }
        Ok(Self {
            store: vec![0_u8; size],
            sections: vec![Section {
                name: "blank".to_string(),
                virtual_address: 0,
                virtual_size: size as u32,
                raw: 0,
                raw_size: size,
                characteristics: 0xc000_0000,
            }],
            entry_point: IMAGE_BASE,
            size_of_image: size as u32,
            imports: Vec::new(),
        })
    }

    /// Fills an import slot the way the Windows loader does: before the image
    /// executes, the loader writes DLL addresses into the IAT even though the
    /// section holding it is read-only once the program runs. Only addresses the
    /// image declares as import slots may be written this way, so this cannot be
    /// used to patch arbitrary read-only data.
    pub fn write_import(&mut self, va: u32, value: u32) -> Result<()> {
        if !self.imports.iter().any(|import| import.iat_va == va) {
            return Err(format!("{va:#010x} is not an import slot"));
        }
        let start = self.offset(va, 4)?;
        self.store[start..start + 4].copy_from_slice(&value.to_le_bytes());
        Ok(())
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    /// The whole guest address space, starting at [`IMAGE_BASE`]. The wasm host
    /// hands this to JavaScript so it can read pixels and diagnose memory
    /// without a copy.
    pub fn as_slice(&self) -> &[u8] {
        &self.store
    }

    /// The same window, mutably, for a host routine that walks guest buffers
    /// itself ([`crate::hle`]). The routine bounds its own accesses against the
    /// slice, so a bad guest pointer becomes an error there rather than a panic.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.store
    }

    /// Start of the mapping in guest addresses, i.e. what `as_slice()[0]` is.
    pub const fn base(&self) -> u32 {
        IMAGE_BASE
    }

    /// End of the mapped image, which is where `VirtualAlloc`'s free region
    /// starts in the guest's address space.
    pub fn image_end(&self) -> u32 {
        IMAGE_BASE + self.size_of_image
    }

    fn unmapped(va: u32, len: usize) -> String {
        format!("guest memory: {len} byte(s) at {va:#010x} are not mapped")
    }

    fn offset(&self, va: u32, len: usize) -> Result<usize> {
        let start = va.checked_sub(IMAGE_BASE).ok_or_else(|| Self::unmapped(va, len))? as usize;
        let end = start.checked_add(len).ok_or_else(|| Self::unmapped(va, len))?;
        if end > self.store.len() {
            return Err(Self::unmapped(va, len));
        }
        Ok(start)
    }

    fn check_writable(&self, va: u32, len: usize) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        if va < self.image_end() {
            let Some(rva) = va.checked_sub(IMAGE_BASE) else {
                return Err(Self::unmapped(va, len));
            };
            let last = rva as u64 + len as u64 - 1;
            let Some(section) = self.sections.iter().find(|section| section.contains(rva)) else {
                return Err(format!(
                    "guest memory: write to the read-only image headers at {va:#010x}"
                ));
            };
            if !section.is_writable() || last >= section.end() as u64 {
                return Err(format!(
                    "guest memory: write to read-only {} at {va:#010x}",
                    section.name
                ));
            }
            return Ok(());
        }
        if (TEB_BASE as u64..MEMORY_TOP as u64).contains(&(va as u64))
            && va as u64 + len as u64 <= MEMORY_TOP as u64
        {
            return Ok(());
        }
        Err(format!(
            "guest memory: write to unmapped {va:#010x} (memory ends at {MEMORY_TOP:#010x})"
        ))
    }

    pub fn read_bytes(&self, va: u32, len: usize) -> Result<&[u8]> {
        let start = self.offset(va, len)?;
        Ok(&self.store[start..start + len])
    }

    pub fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Result<()> {
        self.check_writable(va, bytes.len())?;
        let start = self.offset(va, bytes.len())?;
        self.store[start..start + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }

    /// Copies `len` bytes from one guest address to another without leaving guest
    /// memory.
    ///
    /// The surface blits and the flip do this once per row, hundreds of times per
    /// frame, and going through a `Vec` for each row cost an allocation and a copy
    /// apiece. Both ends are still bounds- and permission-checked: the source may
    /// be read-only code, the destination may not.
    pub fn copy(&mut self, from: u32, to: u32, len: usize) -> Result<()> {
        if len == 0 || from == to {
            return Ok(());
        }
        let source = self.offset(from, len)?;
        self.check_writable(to, len)?;
        let target = self.offset(to, len)?;
        self.store.copy_within(source..source + len, target);
        Ok(())
    }

    pub fn read_u8(&self, va: u32) -> Result<u8> {
        Ok(self.read_bytes(va, 1)?[0])
    }

    pub fn write_u8(&mut self, va: u32, value: u8) -> Result<()> {
        self.write_bytes(va, &[value])
    }

    pub fn read_u16(&self, va: u32) -> Result<u16> {
        Ok(u16::from_le_bytes(self.read_bytes(va, 2)?.try_into().unwrap()))
    }

    pub fn write_u16(&mut self, va: u32, value: u16) -> Result<()> {
        self.write_bytes(va, &value.to_le_bytes())
    }

    pub fn read_u32(&self, va: u32) -> Result<u32> {
        Ok(u32::from_le_bytes(self.read_bytes(va, 4)?.try_into().unwrap()))
    }

    pub fn read_i32(&self, va: u32) -> Result<i32> {
        Ok(self.read_u32(va)? as i32)
    }

    pub fn write_u32(&mut self, va: u32, value: u32) -> Result<()> {
        self.write_bytes(va, &value.to_le_bytes())
    }

    /// Reads a NUL-terminated string, for shims that receive `LPCSTR`.
    pub fn read_cstr(&self, va: u32) -> Result<String> {
        let mut bytes = Vec::new();
        for offset in 0..MAX_CSTRING {
            let byte = self.read_u8(va.wrapping_add(offset as u32))?;
            if byte == 0 {
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
            bytes.push(byte);
        }
        Err(format!("string at {va:#010x} is not terminated"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference() -> Option<GuestMemory> {
        let image = crate::reference_image()?;
        Some(GuestMemory::load(image).expect("image maps"))
    }

    #[test]
    fn maps_the_image_at_its_fixed_base() {
        let Some(memory) = reference() else {
            return;
        };
        assert_eq!(memory.read_bytes(IMAGE_BASE, 2).unwrap(), b"MZ");
        assert_eq!(memory.entry_point, 0x004a_e604);
        assert_eq!(memory.image_end(), 0x004d_6000);

        // The entry point holds the CRT startup stub's first instruction, and it
        // must equal what the file holds at that RVA.
        let image = crate::reference_image().unwrap();
        let entry_rva = memory.entry_point - IMAGE_BASE;
        let offset = image.rva_to_offset(entry_rva).unwrap();
        assert_eq!(
            memory.read_bytes(memory.entry_point, 4).unwrap(),
            &image.bytes[offset..offset + 4]
        );
    }

    #[test]
    fn zero_fills_virtual_only_space() {
        let Some(memory) = reference() else {
            return;
        };
        // DSEG declares 0xe0 virtual bytes over a full raw page; the tail of the
        // section exists only as zero-filled memory.
        let dseg = memory
            .sections()
            .iter()
            .find(|section| section.name == "DSEG")
            .unwrap();
        assert_eq!(
            memory.read_bytes(IMAGE_BASE + dseg.virtual_address + 0xe0, 0x20).unwrap(),
            &[0_u8; 0x20]
        );
    }

    #[test]
    fn enforces_section_permissions() {
        let Some(mut memory) = reference() else {
            return;
        };
        let text = memory.sections()[0].clone();
        assert!(memory.write_u8(IMAGE_BASE + text.virtual_address, 0xcc).is_err());
        assert!(memory.write_u8(IMAGE_BASE + 4, 0).is_err());

        let data = memory
            .sections()
            .iter()
            .find(|section| section.name == ".data")
            .unwrap()
            .clone();
        let data_va = IMAGE_BASE + data.virtual_address;
        memory.write_u32(data_va, 0x1234_5678).unwrap();
        assert_eq!(memory.read_u32(data_va).unwrap(), 0x1234_5678);

        // A write that starts in a writable section but runs past its end must
        // still be rejected.
        assert!(memory
            .write_bytes(IMAGE_BASE + data.end() - 2, &[0; 4])
            .is_err());
    }

    #[test]
    fn faults_outside_the_mapped_range() {
        let Some(memory) = reference() else {
            return;
        };
        assert!(memory.read_u8(0).is_err());
        assert!(memory.read_u32(IMAGE_BASE - 4).is_err());
        assert!(memory.read_u8(MEMORY_TOP).is_err());
        assert!(memory.read_bytes(IMAGE_BASE, usize::MAX).is_err());
        assert!(memory.read_cstr(0).is_err());
    }

    #[test]
    fn a_blank_memory_is_writable_everywhere() {
        let mut memory = GuestMemory::empty(0x2000).unwrap();
        assert_eq!(memory.sections()[0].name, "blank");
        memory.write_u8(IMAGE_BASE, 0x90).unwrap();
        memory.write_u32(IMAGE_BASE + 0x1ffc, 0xdead_beef).unwrap();
        assert_eq!(memory.read_u32(IMAGE_BASE + 0x1ffc).unwrap(), 0xdead_beef);
        assert_eq!(memory.as_slice()[0], 0x90);
        assert_eq!(memory.base(), IMAGE_BASE);
        assert!(memory.write_u8(IMAGE_BASE + 0x2000, 0).is_err());
        assert!(GuestMemory::empty(0).is_err());
    }

    #[test]
    fn sets_up_teb_and_stack() {
        let Some(mut memory) = reference() else {
            return;
        };
        assert_eq!(memory.read_u32(TEB_BASE + TIB_EXCEPTION_LIST).unwrap(), u32::MAX);
        assert_eq!(memory.read_u32(TEB_BASE + TIB_STACK_BASE).unwrap(), STACK_TOP);
        assert_eq!(memory.read_u32(TEB_BASE + TIB_STACK_LIMIT).unwrap(), STACK_BASE);
        assert_eq!(memory.read_u32(TEB_BASE + TIB_SELF).unwrap(), TEB_BASE);

        memory.write_u32(STACK_TOP - 4, 0x1234).unwrap();
        assert_eq!(memory.read_u32(STACK_TOP - 4).unwrap(), 0x1234);
        assert!(memory.write_u32(HEAP_BASE, 1).is_ok());
    }
}
