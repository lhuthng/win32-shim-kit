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
///
/// The arena is one flat reservation, so how much a guest can hold is decided
/// here. Little Fighter 2 draws every sprite through a `CreateDIBSection` at 32
/// bits per pixel, whether or not the file it came from was, and its mods ship
/// the whole system sprite set in `sprite/sys`: 655 MiB of pixels for NTSD
/// Community alone. Decoding a stage on top of that reaches 1155 MiB of bitmaps
/// held at once -- 1.45 copies of every picture in the install -- because the
/// loading screen's decode stays live beside the fight's. A 32-bit Windows
/// process has 2 GiB for exactly that reason, and this is as close as the
/// browser target can come: guest memory is one `Vec<u8>`, and a `Vec` may not
/// exceed `isize::MAX` bytes, which puts the ceiling just under 2 GiB. The
/// reserve below leaves a megabyte of that headroom, so `MEMORY_TOP` is the
/// largest arena this shape can have -- a guest that needs more needs the
/// native host, not a bigger number.
pub const HEAP_BASE: u32 = STACK_TOP;
pub const HEAP_RESERVE: u32 = 0x7f20_0000;
pub const MEMORY_TOP: u32 = HEAP_BASE + HEAP_RESERVE;

/// `NT_TIB`, which is what `fs:` refers to in this image.
const TIB_EXCEPTION_LIST: u32 = 0x00;
const TIB_STACK_BASE: u32 = 0x04;
const TIB_STACK_LIMIT: u32 = 0x08;
const TIB_SELF: u32 = 0x18;

const MAX_CSTRING: usize = 4096;

/// The words `SHIMKIT_WATCH_WRITE` names, or none.
fn watched_writes() -> Vec<u32> {
    let Ok(text) = std::env::var("SHIMKIT_WATCH_WRITE") else {
        return Vec::new();
    };
    text.split(',')
        .filter_map(|part| u32::from_str_radix(part.trim().trim_start_matches("0x"), 16).ok())
        .collect()
}

/// `SHIMKIT_WATCH_RANGE=start-end` (hex, inclusive-exclusive) watches a whole
/// buffer rather than a handful of words. Unlike the word watch, it also records
/// a byte coverage map and a tally of the guest instructions that wrote there,
/// so a buffer with holes can say whether the holes were stored as zero or were
/// never stored at all, and which routine did the storing.
fn watched_ranges() -> Vec<(u32, u32)> {
    let Ok(text) = std::env::var("SHIMKIT_WATCH_RANGE") else {
        return Vec::new();
    };
    text.split(',')
        .filter_map(|part| {
            let (start, end) = part.trim().split_once('-')?;
            let hex = |s: &str| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16);
            let (start, end) = (hex(start).ok()?, hex(end).ok()?);
            (start < end).then_some((start, end))
        })
        .collect()
}

pub struct GuestMemory {
    store: Vec<u8>,
    sections: Vec<Section>,
    pub entry_point: u32,
    pub size_of_image: u32,
    pub imports: Vec<crate::pe::Import>,
    /// Guest DLL images mapped beside the program, as `(start, end)`.
    ///
    /// The program's own image is `[IMAGE_BASE, image_end())`, whose sections
    /// decide what may be written. A module lives near the top of the arena,
    /// where the heap cannot reach it until it has spent every byte it has, and
    /// is the guest's to write as it likes: its `.data` is the reason it is
    /// loaded at all.
    modules: Vec<(u32, u32)>,
    /// The instruction the guest is running and its stack pointer, so a watched
    /// store can name the code that made it. The session writes these before
    /// every step, and only does so while [`Self::watch`] is set.
    pc: u32,
    esp: u32,
    /// The words `SHIMKIT_WATCH_WRITE` names. A guest global that changes with no
    /// code in the image writing it is the mark of a copy that ran past its
    /// buffer, and the address it lands on says nothing about the instruction
    /// that did it -- so the store is reported as it happens instead.
    watch: Vec<u32>,
    watch_hits: u32,
    /// The last few watched stores, printed when the guest stops: the store that
    /// made the value bad is the last one before it, and a run of 90 million
    /// instructions has far more of them than a log can hold.
    watch_recent: std::collections::VecDeque<String>,
    /// `SHIMKIT_WATCH_RANGE` ranges. Only the first gets a coverage map: one
    /// buffer is enough to answer "which bytes did the guest never write".
    ranges: Vec<(u32, u32)>,
    cover: Vec<u8>,
    cover_eips: std::collections::BTreeMap<u32, u64>,
    range_hits: u64,
    range_zero_hits: u64,
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
            ..
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
            modules: Vec::new(),
            pc: 0,
            esp: 0,
            watch: watched_writes(),
            watch_hits: 0,
            watch_recent: Default::default(),
            ranges: watched_ranges(),
            cover: Vec::new(),
            cover_eips: Default::default(),
            range_hits: 0,
            range_zero_hits: 0,
        };
        memory.init_cover();
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
        let mut memory = Self {
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
            modules: Vec::new(),
            pc: 0,
            esp: 0,
            watch: watched_writes(),
            watch_hits: 0,
            watch_recent: Default::default(),
            ranges: watched_ranges(),
            cover: Vec::new(),
            cover_eips: Default::default(),
            range_hits: 0,
            range_zero_hits: 0,
        };
        memory.init_cover();
        Ok(memory)
    }

    /// Allocates the coverage bitmap after the struct exists, so both load paths
    /// share the same rule: the first watched range gets one byte per address.
    fn init_cover(&mut self) {
        if let Some(&(start, end)) = self.ranges.first() {
            self.cover = vec![0; (end - start) as usize];
        }
    }

    /// The bytes of the first watched range that guest writes have touched, one
    /// byte per address. Zero means nothing ever stored there.
    pub fn coverage(&self) -> Option<(u32, &[u8])> {
        self.ranges.first().map(|&(start, _)| (start, self.cover.as_slice()))
    }

    /// Whether either watch is armed, which is what makes the session bother to
    /// record the instruction pointer.
    pub fn watching(&self) -> bool {
        !self.watch.is_empty() || !self.ranges.is_empty()
    }

    /// Records the instruction the guest is about to run, so a watched store can
    /// name it. Free unless a watch is set.
    #[inline]
    pub fn set_pc(&mut self, pc: u32, esp: u32) {
        if self.watching() {
            self.pc = pc;
            self.esp = esp;
        }
    }

    /// Tallies a store inside a watched range by the guest instruction that made
    /// it and marks the bytes it covered. Kept separate from the word watch so a
    /// 64 KiB buffer can be watched without printing a line per byte.
    #[inline]
    fn watch_range_store(&mut self, va: u32, len: usize) {
        if self.ranges.is_empty() {
            return;
        }
        let store_end = (va as u64).saturating_add(len as u64);
        let mut touched = false;
        for &(start, end) in &self.ranges {
            let lo = (va as u64).max(start as u64);
            let hi = store_end.min(end as u64);
            if lo >= hi {
                continue;
            }
            touched = true;
            *self.cover_eips.entry(self.pc).or_insert(0) += hi - lo;
            let first = self.ranges[0];
            if start == first.0 {
                let begin = (lo - start as u64) as usize;
                let finish = (hi - start as u64) as usize;
                if let Some(bytes) = self.cover.get_mut(begin..finish) {
                    bytes.fill(1);
                }
            }
            let at = (va.wrapping_sub(IMAGE_BASE)) as usize;
            if let Some(bytes) = self.store.get(at..at.saturating_add(len.min(64))) {
                if bytes.iter().all(|&byte| byte == 0) {
                    self.range_zero_hits += 1;
                }
            }
        }
        if touched {
            self.range_hits += 1;
            if self.range_hits <= 40 {
                println!(
                    "range: {len} byte(s) at {va:#010x} by {:#010x}",
                    self.pc
                );
            }
        }
    }

    /// Reports a store that touches a watched word, with the value it left, the
    /// instruction that made it and the words on the stack above it -- usually the
    /// caller's return address, which is what says *which* copy overflowed.
    #[inline]
    fn watch_store(&mut self, va: u32, len: usize) {
        if self.ranges.is_empty() && self.watch.is_empty() {
            return;
        }
        self.watch_range_store(va, len);
        let end = va as u64 + len as u64;
        if !self
            .watch
            .iter()
            .any(|&at| u64::from(va) < u64::from(at) + 4 && u64::from(at) < end)
        {
            return;
        }
        self.watch_hits += 1;
        let mut value = String::new();
        let start = va.wrapping_sub(IMAGE_BASE) as usize;
        if let Some(bytes) = self.store.get(start..start.saturating_add(len.min(8))) {
            for byte in bytes {
                value.push_str(&format!("{byte:02x}"));
            }
        }
        let mut stack = Vec::new();
        for index in 0..8 {
            let at = self.esp.wrapping_add(index * 4);
            let Ok(word) = self.read_u32(at) else { break };
            stack.push(format!("{word:#010x}"));
        }
        let line = format!(
            "watch: {len} byte(s) at {va:#010x} = {value} by {:#010x}",
            self.pc
        );
        if self.watch_hits <= 300 {
            println!("{line}\n  stack+0..8: {}", stack.join(" "));
        }
        self.watch_recent.push_back(format!("{line}"));
        if self.watch_recent.len() > 48 {
            self.watch_recent.pop_front();
        }
    }

    /// Prints the last watched stores and, for a range watch, the guest
    /// instructions that wrote into it. A guest that stops on a value nothing in
    /// the image wrote needs the stores that led to it, not the last 300 of them.
    pub fn dump_watch(&self) {
        for line in &self.watch_recent {
            println!("{line}");
        }
        if !self.ranges.is_empty() {
            println!(
                "range watches: {:?}, stores {}, all-zero stores {}, distinct writers {}",
                self.ranges, self.range_hits, self.range_zero_hits, self.cover_eips.len()
            );
            let mut writers: Vec<_> = self.cover_eips.iter().collect();
            writers.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
            for (pc, count) in writers.into_iter().take(60) {
                println!("  writer {pc:#010x}: {count} bytes");
            }
            let covered = self.cover.iter().filter(|&&byte| byte != 0).count();
            println!(
                "  coverage {}/{} bytes ({:.1}%)",
                covered,
                self.cover.len(),
                covered as f64 * 100.0 / self.cover.len().max(1) as f64
            );
        }
    }

    /// Maps a guest DLL image at `base`, the address its header declares.
    ///
    /// The bytes arrive already relocated: the payload's compiler
    /// (`shimkit-link`) placed the module in this window and adjusted every
    /// absolute address, so the only work here is the copy. That is deliberate --
    /// the interpreter never runs a relocation pass, so a module that could not
    /// be moved is refused by the compiler rather than mis-mapped here.
    pub fn map_module(&mut self, image: &Image, base: u32) -> Result<()> {
        if base & 0xfff != 0 {
            return Err(format!("module base {base:#010x} is not page aligned"));
        }
        let end = base
            .checked_add(image.size_of_image)
            .ok_or_else(|| format!("module at {base:#010x} wraps the address space"))?;
        if base < self.image_end() {
            return Err(format!(
                "module at {base:#010x} overlaps the program, which ends at {:#010x}",
                self.image_end()
            ));
        }
        if end > MEMORY_TOP {
            return Err(format!(
                "module at {base:#010x} ends at {end:#010x}, past the end of guest memory at \
                 {MEMORY_TOP:#010x}"
            ));
        }
        for (start, stop) in &self.modules {
            if base < *stop && *start < end {
                return Err(format!(
                    "module at {base:#010x} overlaps the one at {start:#010x}"
                ));
            }
        }
        if image.size_of_headers as usize > image.bytes.len() {
            return Err("the module's headers are shorter than it declares".into());
        }
        let offset = |va: u32| (va - IMAGE_BASE) as usize;
        let headers = image.size_of_headers as usize;
        self.store[offset(base)..offset(base) + headers]
            .copy_from_slice(&image.bytes[..headers]);
        for section in &image.sections {
            if section.raw_size == 0 {
                continue;
            }
            let from = section
                .raw
                .checked_add(section.raw_size)
                .filter(|end| *end <= image.bytes.len())
                .ok_or_else(|| format!("{} reads past the end of the module", section.name))?;
            let at = offset(base.wrapping_add(section.virtual_address));
            self.store[at..at + section.raw_size]
                .copy_from_slice(&image.bytes[section.raw..from]);
        }
        self.modules.push((base, end));
        Ok(())
    }

    /// Records a guest DLL's import slots, so the session may fill them the way
    /// it fills the program's own.
    pub fn add_imports(&mut self, imports: Vec<crate::pe::Import>) {
        self.imports.extend(imports);
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
        if !self.watch.is_empty() && self.watch_hits <= 200 {
            println!(
                "watch: guest memory handed out writable to {:#010x} (esp {:#010x})",
                self.pc, self.esp
            );
        }
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
        // A guest DLL's image is writable: its `.data` is what it was loaded for,
        // and its code is not written by anything here.
        for (start, stop) in &self.modules {
            if va >= *start && va as u64 + len as u64 <= u64::from(*stop) {
                return Ok(());
            }
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

    /// The tail of memory from `va`, when the address maps. Reads narrower than
    /// the tail run their own bounds check against its length, so the single
    /// word and byte reads below cost one subtraction and two compares instead
    /// of a call into [`Self::read_bytes`] with a `Result` and a slice.
    ///
    /// The interpreter retires tens of millions of loads a second through here,
    /// which is why the plain case is written out rather than reasoned about.
    #[inline]
    fn tail(&self, va: u32) -> Option<&[u8]> {
        self.store.get(va.wrapping_sub(IMAGE_BASE) as usize..)
    }

    /// The same as [`Self::tail`], for a write. Nothing under `IMAGE_BASE` and
    /// nothing past [`MEMORY_TOP`] maps, and the image is checked against its
    /// sections by [`Self::check_writable`] instead.
    #[inline]
    fn tail_mut(&mut self, va: u32) -> Option<&mut [u8]> {
        self.store
            .get_mut(va.wrapping_sub(IMAGE_BASE) as usize..)
    }

    /// Whether a plain store at `va` is allowed without consulting the section
    /// table: above the TEB, which is where the stack and the heap live.
    ///
    /// The same range [`Self::check_writable`] accepts, so the fast path answers
    /// what the slow one would: below it is the image, whose sections decide, and
    /// between the image and the TEB nothing is mapped at all.
    #[inline]
    fn unchecked_write(va: u32, len: usize) -> bool {
        va >= TEB_BASE && va as u64 + len as u64 <= MEMORY_TOP as u64
    }

    #[inline]
    pub fn write_bytes(&mut self, va: u32, bytes: &[u8]) -> Result<()> {
        let len = bytes.len();
        if Self::unchecked_write(va, len) {
            if let Some(target) = self.tail_mut(va) {
                if target.len() >= len {
                    target[..len].copy_from_slice(bytes);
                    self.watch_store(va, len);
                    return Ok(());
                }
            }
        }
        self.check_writable(va, len)?;
        let start = self.offset(va, len)?;
        self.store[start..start + len].copy_from_slice(bytes);
        self.watch_store(va, len);
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
        self.watch_store(to, len);
        Ok(())
    }

    #[inline]
    pub fn read_u8(&self, va: u32) -> Result<u8> {
        if let Some(tail) = self.tail(va) {
            if let Some(first) = tail.first() {
                return Ok(*first);
            }
        }
        Ok(self.read_bytes(va, 1)?[0])
    }

    #[inline]
    pub fn write_u8(&mut self, va: u32, value: u8) -> Result<()> {
        if Self::unchecked_write(va, 1) {
            if let Some(target) = self.tail_mut(va) {
                if let Some(first) = target.first_mut() {
                    *first = value;
                    // The fast path stores straight into the tail, so it has to
                    // report the store itself: `write_bytes` is the other branch
                    // and the only one that would. Without this a watched write
                    // through a `mov [mem], reg` -- which is most of them -- goes
                    // unseen, and a watch armed on a value that is visibly moving
                    // reports nothing at all.
                    self.watch_store(va, 1);
                    return Ok(());
                }
            }
        }
        self.write_bytes(va, &[value])
    }

    #[inline]
    pub fn read_u16(&self, va: u32) -> Result<u16> {
        if let Some(tail) = self.tail(va) {
            if tail.len() >= 2 {
                return Ok(u16::from_le_bytes([tail[0], tail[1]]));
            }
        }
        Ok(u16::from_le_bytes(self.read_bytes(va, 2)?.try_into().unwrap()))
    }

    #[inline]
    pub fn write_u16(&mut self, va: u32, value: u16) -> Result<()> {
        if Self::unchecked_write(va, 2) {
            if let Some(target) = self.tail_mut(va) {
                if target.len() >= 2 {
                    target[..2].copy_from_slice(&value.to_le_bytes());
                    self.watch_store(va, 2);
                    return Ok(());
                }
            }
        }
        self.write_bytes(va, &value.to_le_bytes())
    }

    #[inline]
    pub fn read_u32(&self, va: u32) -> Result<u32> {
        if let Some(tail) = self.tail(va) {
            if tail.len() >= 4 {
                return Ok(u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]));
            }
        }
        Ok(u32::from_le_bytes(self.read_bytes(va, 4)?.try_into().unwrap()))
    }

    #[inline]
    pub fn read_i32(&self, va: u32) -> Result<i32> {
        Ok(self.read_u32(va)? as i32)
    }

    #[inline]
    pub fn write_u32(&mut self, va: u32, value: u32) -> Result<()> {
        if Self::unchecked_write(va, 4) {
            if let Some(target) = self.tail_mut(va) {
                if target.len() >= 4 {
                    target[..4].copy_from_slice(&value.to_le_bytes());
                    self.watch_store(va, 4);
                    return Ok(());
                }
            }
        }
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

    /// A watched store is reported whichever path wrote it.
    ///
    /// `write_u8`/`write_u16`/`write_u32` have a fast path that stores straight
    /// into the tail and returns, and it used to return without reporting. A
    /// guest's ordinary `mov [mem], reg` takes that path, so a watch armed on a
    /// value that was visibly moving reported nothing at all -- which reads as
    /// "nothing writes this" and sends a search the wrong way entirely.
    #[test]
    fn a_watched_store_is_reported_on_the_fast_path() {
        let Ok(mut memory) = GuestMemory::empty(HEAP_BASE as usize + 0x1000) else {
            return;
        };
        // Above the TEB, so `unchecked_write` holds and the fast path is the one
        // taken; the tail branch is the one that used to go unreported.
        let at = TEB_BASE;
        memory.watch.push(at);

        memory.write_u8(at, 0x11).unwrap();
        memory.write_u16(at, 0x2233).unwrap();
        memory.write_u32(at, 0x4455_6677).unwrap();

        assert_eq!(memory.watch_hits, 3, "every store is reported");
        assert_eq!(memory.read_u32(at).unwrap(), 0x4455_6677);
    }
}
