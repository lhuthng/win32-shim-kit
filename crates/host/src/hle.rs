//! Guest routines this host runs natively.
//!
//! Each replacement keeps the routine's contract exactly: the same argument
//! order, the same stack cleanup, the same values left in guest memory. The
//! guest never knows the difference, so everything built on top of these
//! routines -- the decoder's callers, the images it produces -- is unchanged.
//!
//! The LZW decoder was the first one. It is pure computation on guest memory
//! with no imports, so it can be replaced without touching the guest's own
//! control flow.
//!
//! The sprite blitters are the second. On a moving frame the game redraws the
//! board through them, and the keyed variant copies one pixel per loop
//! iteration with ten guest instructions behind each byte: fetch, palette
//! lookup, store, two increments and a `loop`. That inner loop is the largest
//! single consumer of guest instructions during movement, so the host runs the
//! whole row decoder instead and touches the same pixels.

use crate::cpu::Cpu;
use crate::memory::GuestMemory;
use crate::pe::IMAGE_BASE;
use crate::Result;

/// The LZW decoder at `0x00409247`: `lzw_decode(ctx, &source_cursor, dst)` with
/// `ctx` in `ecx` and a stdcall `ret 8`.
pub const LZW_DECODE: u32 = 0x0040_9247;

/// Fixed 14-bit code that ends a stream.
const END_CODE: u32 = 0x3fff;
/// Codes below this are literals; the dictionary starts here.
const FIRST_CODE: usize = 0x100;
/// The decoder stops adding dictionary entries at this code.
const LAST_ENTRY: usize = 0x3ffe;
/// The decoder gives up on a chain longer than this many links.
const MAX_CHAIN: u32 = 0xfa0;
/// The guest allocates 0xfa0 bytes for the decode scratch buffer and can write
/// one byte past it before noticing the overlong chain; one spare byte past
/// that keeps the local buffer faithful without trusting the chain.
const SCRATCH_BYTES: usize = MAX_CHAIN as usize + 2;

/// Bit reservoir for the fixed 14-bit codes. Bits arrive most significant
/// first; `bits` holds them at the top of the word and `count` says how many
/// are valid, exactly as the guest keeps them at `ctx+0x30`/`ctx+0x34`.
struct Reservoir {
    bits: u32,
    count: u32,
}

/// The decoder's private state. The guest allocates the same three tables once
/// and reuses them across calls without clearing them, so the host does too:
/// the arrays are per-session, not per-call.
pub struct Lzw {
    entry_code: Vec<u32>,
    entry_byte: Vec<u8>,
    scratch: Vec<u8>,
}

impl Default for Lzw {
    fn default() -> Self {
        Self::new()
    }
}

impl Lzw {
    pub fn new() -> Self {
        Self {
            entry_code: vec![0; LAST_ENTRY + 2],
            entry_byte: vec![0; LAST_ENTRY + 2],
            scratch: vec![0; SCRATCH_BYTES],
        }
    }

    /// Decodes one stream and returns the guest work the call stands for, in
    /// retired-instruction equivalents. The caller adds that to the guest's
    /// instruction count so timers, budgets and the profile stay comparable.
    pub fn decode(&mut self, memory: &mut GuestMemory, cpu: &mut Cpu) -> Result<u32> {
        let esp = cpu.regs.esp;
        let return_address = memory.read_u32(esp)?;
        let source_va = memory.read_u32(esp.wrapping_add(4))?;
        let dst_va = memory.read_u32(esp.wrapping_add(8))?;
        let ctx_va = cpu.regs.ecx;
        let mut work = 8;

        let src_start = offset_of(source_va, "source")?;
        let dst_start = offset_of(dst_va, "destination")?;
        let store = memory.as_mut_slice();
        if src_start >= store.len() {
            return Err(format!(
                "LZW source {source_va:#010x} is outside guest memory"
            ));
        }

        let mut src = src_start;
        let mut reservoir = Reservoir { bits: 0, count: 0 };
        let mut next = FIRST_CODE;
        let mut previous = read_code(store, &mut reservoir, &mut src, &mut work)
            .ok_or_else(|| format!("LZW source ran past the end of guest memory at {:#010x}", IMAGE_BASE as u64 + src as u64))?;

        let mut written = 0_usize;
        let mut first_char = previous as u8;
        write_dst(store, dst_start, written, first_char)?;
        written += 1;
        work += 1;

        let status = loop {
            let Some(code) = read_code(store, &mut reservoir, &mut src, &mut work) else {
                return Err(format!(
                    "LZW source ran past the end of guest memory from {source_va:#010x}"
                ));
            };
            if code == END_CODE {
                break 1;
            }

            let mut end = if code < next as u32 {
                match emit(
                    &self.entry_code,
                    &self.entry_byte,
                    &mut self.scratch,
                    0,
                    code,
                    &mut work,
                ) {
                    Some(end) => end,
                    None => break 0,
                }
            } else {
                // KwKwK: the code being defined right here. Its string is the
                // previous string with its own first byte appended, so the
                // scratch holds that first byte and then the previous string.
                self.scratch[0] = first_char;
                match emit(
                    &self.entry_code,
                    &self.entry_byte,
                    &mut self.scratch,
                    1,
                    previous,
                    &mut work,
                ) {
                    Some(end) => end,
                    None => break 0,
                }
            };

            first_char = self.scratch[end];
            loop {
                if let Some(slot) = store.get_mut(dst_start + written) {
                    *slot = self.scratch[end];
                } else {
                    return Err(format!(
                        "LZW output ran past the end of guest memory from {dst_va:#010x}"
                    ));
                }
                written += 1;
                work += 1;
                if end == 0 {
                    break;
                }
                end -= 1;
            }

            if next <= LAST_ENTRY {
                self.entry_code[next] = previous;
                self.entry_byte[next] = first_char;
                next += 1;
                work += 2;
            }
            previous = code;
        };

        // The guest's reader advances its own copy of the argument; the slot
        // stays updated after the call, so the host leaves the same value.
        let advanced = IMAGE_BASE + src as u32;
        memory.write_u32(esp.wrapping_add(4), advanced)?;
        memory.write_u32(ctx_va.wrapping_add(0x30), reservoir.count)?;
        memory.write_u32(ctx_va.wrapping_add(0x34), reservoir.bits)?;

        cpu.regs.eax = status;
        cpu.regs.eip = return_address;
        cpu.regs.esp = esp.wrapping_add(12);
        Ok(work)
    }
}

/// The opaque CSEG sprite blitter at `0x004d28b0` and its palette-keyed twin at
/// `0x004d29c5`. Both are called only from the wrapper at `0x004d14a0` with
/// `eax` pointing at the sprite descriptor, `edx` at the destination surface
/// descriptor and `ebx` at the clipped position rectangle. Neither has stack
/// arguments and neither returns a value, so the only contract is the pixels
/// they write and the registers they may clobber.
pub const BLIT_OPAQUE: u32 = 0x004d_28b0;
pub const BLIT_KEYED: u32 = 0x004d_29c5;

/// The shared rectangle the wrapper clipped before the call. Only three fields
/// are read: the first source row, the destination row, and the row count.
const BLIT_RECT: u32 = 0x004d_00ae;
const BLIT_SRC_Y: u32 = BLIT_RECT + 0x2;
const BLIT_DST_Y: u32 = BLIT_RECT + 0x6;
const BLIT_HEIGHT: u32 = BLIT_RECT + 0xa;
/// Scratch the guest writes and never restores. Nothing outside the blitter
/// reads it, but the host leaves the same value anyway.
const BLIT_CLIP: u32 = 0x004d_00ca;
/// Holds the base of the 256 by 256 colour table the keyed blitter indexes as
/// `table[source << 8 | destination]`.
const BLIT_LUT: u32 = 0x004d_0080;

/// Counters for [`Blitter::blit`], so a run can report how much of a frame the
/// host drew instead of the interpreter.
#[derive(Default)]
pub struct Blitter {
    calls: u64,
    pixels: u64,
}

impl Blitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn pixels(&self) -> u64 {
        self.pixels
    }

    /// Draws one sprite row by row, exactly as the guest routine does, and
    /// returns the guest instructions the call stands for.
    ///
    /// The sprite is a stream of rows. A row starts with a delta word that
    /// locates the next row, then alternating runs: a positive header is a
    /// literal run of that many palette indices, a negative header is a
    /// transparent skip of `-header` pixels. Runs are clipped against the
    /// destination rectangle only; `width` is the sprite's own pixel count and
    /// decides when a row is done, not how much a single run may paint.
    pub fn blit(&mut self, memory: &mut GuestMemory, cpu: &mut Cpu, keyed: bool) -> Result<u32> {
        let esp = cpu.regs.esp;
        let return_address = memory.read_u32(esp)?;

        let sprite = cpu.regs.eax;
        let surface = cpu.regs.edx;
        let position = cpu.regs.ebx;

        let width = u32::from(memory.read_u16(sprite)?);
        let sprite_pixels = memory.read_u32(sprite.wrapping_add(4))?;
        let pitch = memory.read_u16(surface)?;
        let surface_pixels = memory.read_u32(surface.wrapping_add(4))?;
        let dest_x = i32::from(memory.read_u16(position.wrapping_add(4))? as i16);
        let src_y = u32::from(memory.read_u16(BLIT_SRC_Y)?);
        let dest_y = i32::from(memory.read_u16(BLIT_DST_Y)? as i16);
        let height = u32::from(memory.read_u16(BLIT_HEIGHT)?);
        let clip_x = memory.read_u16(surface.wrapping_add(0xc))?;
        let clip_width = memory.read_u16(surface.wrapping_add(0x10))?;
        let table = if keyed { memory.read_u32(BLIT_LUT)? } else { 0 };

        memory.write_u32(BLIT_CLIP, surface.wrapping_add(0xc))?;

        let row_table = sprite_pixels.wrapping_add(src_y.wrapping_mul(2));
        let row_offset = u32::from(memory.read_u16(row_table)?);
        let mut source = sprite_pixels.wrapping_add(row_offset);
        // The guest multiplies two 16-bit words, so the row offset wraps at 32
        // bits the same way; a surface at row zero is the common case.
        let mut row = surface_pixels.wrapping_add(
            dest_y.wrapping_mul(i32::from(pitch as i16)) as u32,
        );

        let store = memory.as_mut_slice();
        let mut work = 6_u32;
        let mut drawn = 0_u64;

        for _ in 0..height {
            let delta = read_guest_u16(store, source, "row delta")?;
            source = source.wrapping_add(2);
            let segments = source;
            let mut consumed = 0_u32;
            let mut x = dest_x;
            let mut cut = false;
            work += 5;

            while consumed < width && !cut {
                let header = read_guest_u16(store, source, "run header")? as i16;
                source = source.wrapping_add(2);
                work += 3;

                if header < 0 {
                    let skip = u32::from(header.unsigned_abs());
                    x = x.wrapping_add(skip as i32);
                    consumed = consumed.wrapping_add(skip);
                    work += 2;
                    continue;
                }

                let run = u32::from(header as u16);
                consumed = consumed.wrapping_add(run);
                work += 2;
                match clip_run(x, run, clip_x, clip_width) {
                    Clipped::Outside { next_x, skip } => {
                        source = source.wrapping_add(skip).wrapping_add(run);
                        x = next_x;
                        work += 4;
                    }
                    Clipped::Inside { left, length, skip, cut_right } => {
                        source = source.wrapping_add(skip);
                        let length = usize::from(length);
                        let length32 = length as u32;
                        let destination = row.wrapping_add(u32::from(left));
                        copy_run(store, source, destination, length, table, keyed)?;
                        drawn += length as u64;
                        work += if keyed {
                            5 + 10 * length32
                        } else {
                            5 + length32.div_ceil(4)
                        };
                        source = source.wrapping_add(length32);
                        x = u32::from(left).wrapping_add(length32) as i32;
                        cut = cut_right;
                    }
                }
            }

            // The delta locates the next row, not the end of the stream, so a
            // row that ends early still resumes at the same place.
            source = segments.wrapping_add(u32::from(delta));
            row = row.wrapping_add(u32::from(pitch));
            work += 3;
        }

        cpu.regs.eip = return_address;
        cpu.regs.esp = esp.wrapping_add(4);
        self.calls += 1;
        self.pixels += drawn;
        Ok(work)
    }
}

/// What the destination clip did to one literal run.
enum Clipped {
    /// Nothing can be drawn; `skip` source pixels were already consumed by a
    /// left clip and the caller skips the rest of the run.
    Outside { next_x: i32, skip: u32 },
    /// `length` pixels start at `left`, after skipping `skip` source pixels.
    /// `cut_right` means the run was shortened at the right edge, which ends
    /// the row.
    Inside { left: u16, length: u16, skip: u32, cut_right: bool },
}

/// `0x004d2b13` in full. Every value is 16-bit and every add wraps there,
/// including the odd case where a left-clipped run is then found to lie wholly
/// past the right edge: the guest restores the unclipped start and length for
/// the caller's bookkeeping, and this does the same.
fn clip_run(x: i32, run: u32, clip_x: u16, clip_width: u16) -> Clipped {
    let mut left = x as u16;
    let mut length = run as u16;
    let mut skip = 0_u32;
    loop {
        if (left as i16) < (clip_x as i16) {
            let missing = clip_x.wrapping_sub(left);
            if length <= missing {
                return Clipped::Outside { next_x: x.wrapping_add(run as i32), skip };
            }
            length = length.wrapping_sub(missing);
            left = left.wrapping_add(missing);
            skip = skip.wrapping_add(u32::from(missing));
            continue;
        }
        let end = left.wrapping_add(length);
        let limit = clip_x.wrapping_add(clip_width);
        if end <= limit {
            return Clipped::Inside { left, length, skip, cut_right: false };
        }
        let overflow = end.wrapping_sub(limit);
        if length <= overflow {
            return Clipped::Outside { next_x: x.wrapping_add(run as i32), skip };
        }
        return Clipped::Inside {
            left,
            length: length.wrapping_sub(overflow),
            skip,
            cut_right: true,
        };
    }
}

/// Copies one clipped literal run. The opaque routine is a `rep movsb`, so a
/// `memmove` is the same thing; the keyed routine maps every pixel through the
/// colour table as `table[source << 8 | destination]`.
fn copy_run(
    store: &mut [u8],
    source: u32,
    destination: u32,
    length: usize,
    table: u32,
    keyed: bool,
) -> Result<()> {
    let from = guest_offset(store, source, length, "sprite row")?;
    let to = guest_offset(store, destination, length, "surface")?;
    if keyed {
        let base = guest_offset(store, table, 0x1_0000, "colour table")?;
        keyed_copy(store, from, to, length, base);
    } else {
        store.copy_within(from..from + length, to);
    }
    Ok(())
}

/// The keyed loop needs the source, the destination and the table at once.
/// They are disjoint allocations in practice (image data, a surface, a palette
/// bank), and the caller has bounds-checked all three, so one pointer walk is
/// sound here where three overlapping slices could not be borrowed.
fn keyed_copy(store: &mut [u8], source: usize, destination: usize, length: usize, table: usize) {
    let base = store.as_mut_ptr();
    // SAFETY: `source + length`, `destination + length` and `table + 0x10000`
    // were all checked against `store.len()` by `guest_offset`, `store` is not
    // reallocated while the loop runs, and the table index is a byte.
    unsafe {
        let lut = base.add(table);
        for index in 0..length {
            let pixel = *base.add(source + index);
            let under = *base.add(destination + index);
            *base.add(destination + index) =
                *lut.add((usize::from(pixel) << 8) | usize::from(under));
        }
    }
}

fn read_guest_u16(store: &[u8], va: u32, what: &str) -> Result<u16> {
    let at = guest_offset(store, va, 2, what)?;
    Ok(u16::from_le_bytes([store[at], store[at + 1]]))
}

fn guest_offset(store: &[u8], va: u32, length: usize, what: &str) -> Result<usize> {
    let offset = va
        .checked_sub(IMAGE_BASE)
        .ok_or_else(|| format!("sprite blit {what} {va:#010x} is below the image base"))?
        as usize;
    let end = offset
        .checked_add(length)
        .ok_or_else(|| format!("sprite blit {what} at {va:#010x} wraps"))?;
    if end > store.len() {
        return Err(format!(
            "sprite blit {what} {va:#010x} with {length} byte(s) is outside guest memory"
        ));
    }
    Ok(offset)
}

fn offset_of(va: u32, what: &str) -> Result<usize> {
    va.checked_sub(IMAGE_BASE)
        .map(|offset| offset as usize)
        .ok_or_else(|| format!("LZW {what} {va:#010x} is below the image base"))
}

fn write_dst(store: &mut [u8], dst: usize, written: usize, byte: u8) -> Result<()> {
    match store.get_mut(dst + written) {
        Some(slot) => {
            *slot = byte;
            Ok(())
        }
        None => Err(format!(
            "LZW output runs past the end of guest memory from {:#010x}",
            IMAGE_BASE as u64 + dst as u64
        )),
    }
}

/// Reads one 14-bit code, refilling the reservoir eight bits at a time while it
/// holds fewer than 25. The guest emits its most significant bits first, so a
/// byte joins below the valid bits and a code is the top 14.
fn read_code(
    store: &[u8],
    reservoir: &mut Reservoir,
    src: &mut usize,
    work: &mut u32,
) -> Option<u32> {
    while reservoir.count <= 0x18 {
        let byte = *store.get(*src)?;
        *src += 1;
        reservoir.bits |= u32::from(byte) << (0x18 - reservoir.count);
        reservoir.count += 8;
        *work += 1;
    }
    let code = reservoir.bits >> 18;
    reservoir.bits <<= 14;
    reservoir.count -= 14;
    Some(code)
}

/// Expands `code` backwards into `scratch` starting at `at`, and returns the
/// index of its first byte. The guest refuses chains longer than
/// [`MAX_CHAIN`] links, which is a corrupt stream rather than a long one.
fn emit(
    entry_code: &[u32],
    entry_byte: &[u8],
    scratch: &mut [u8],
    mut at: usize,
    mut code: u32,
    work: &mut u32,
) -> Option<usize> {
    let mut links = 0_u32;
    loop {
        if code <= 0xff {
            *scratch.get_mut(at)? = code as u8;
            *work += 1;
            return Some(at);
        }
        *scratch.get_mut(at)? = *entry_byte.get(code as usize)?;
        at += 1;
        code = *entry_code.get(code as usize)?;
        *work += 3;
        if links >= MAX_CHAIN {
            return None;
        }
        links += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::STACK_TOP;
    use crate::session::{Outcome, Session};

    const CTX: u32 = 0x0110_0000;
    const DICT_CODES: u32 = 0x0110_1000;
    const DICT_BYTES: u32 = 0x0111_1000;
    const SCRATCH: u32 = 0x0111_6000;
    const SRC: u32 = 0x0112_0000;
    const DST: u32 = 0x0113_0000;
    const OTHER_DST: u32 = 0x0114_0000;
    const STOP: u32 = 0x0116_0000;
    const SRC_BYTES: usize = 0x4000;

    fn reference() -> Option<Session> {
        let path = std::path::PathBuf::from(std::env::var_os("SHIMKIT_GUEST")?);
        if !path.is_file() {
            return None;
        }
        Some(Session::load(std::fs::read(path).ok()?).unwrap())
    }

    /// A fixed 14-bit, most-significant-first LZW stream, which is the format
    /// the guest decoder reads.
    fn encode(data: &[u8]) -> Vec<u8> {
        let mut codes: Vec<u16> = Vec::new();
        let mut dict = std::collections::HashMap::new();
        let mut next: u16 = 0x100;
        let mut iter = data.iter();
        let Some(&first) = iter.next() else {
            return pack(&[END_CODE as u16]);
        };
        let mut prefix = u16::from(first);
        for &byte in iter {
            if let Some(&code) = dict.get(&(prefix, byte)) {
                prefix = code;
                continue;
            }
            codes.push(prefix);
            if next <= LAST_ENTRY as u16 {
                dict.insert((prefix, byte), next);
                next += 1;
            }
            prefix = u16::from(byte);
        }
        codes.push(prefix);
        codes.push(END_CODE as u16);
        pack(&codes)
    }

    fn pack(codes: &[u16]) -> Vec<u8> {
        let mut bits = 0_u64;
        let mut count = 0_u32;
        let mut out = Vec::new();
        for &code in codes {
            bits = (bits << 14) | u64::from(code);
            count += 14;
            while count >= 8 {
                count -= 8;
                out.push((bits >> count) as u8);
                bits &= (1_u64 << count) - 1;
            }
        }
        if count > 0 {
            out.push((bits << (8 - count)) as u8);
        }
        out
    }

    fn prepare(session: &mut Session, stream: &[u8]) {
        let memory = &mut session.memory;
        memory.write_bytes(DICT_CODES, &vec![0; 0x1_0000]).unwrap();
        memory.write_bytes(DICT_BYTES, &vec![0; 0x4000]).unwrap();
        memory.write_bytes(SCRATCH, &vec![0; 0x1000]).unwrap();
        let mut source = vec![0_u8; SRC_BYTES];
        source[..stream.len()].copy_from_slice(stream);
        memory.write_bytes(SRC, &source).unwrap();
        memory.write_bytes(DST, &vec![0; 0x8000]).unwrap();
        memory.write_bytes(OTHER_DST, &vec![0; 0x8000]).unwrap();
        memory.write_u32(0x004c_ae1c, DICT_CODES).unwrap();
        memory.write_u32(0x004c_ae20, DICT_BYTES).unwrap();
        memory.write_u32(0x004c_ae24, SCRATCH).unwrap();
        memory.write_bytes(STOP, &[0xf4]).unwrap(); // hlt
    }

    /// Enters the decoder the way its callers do. `ecx` carries the context;
    /// the return address is a `hlt`, so a run stops the moment it returns
    /// instead of spinning out its budget.
    fn enter(session: &mut Session, dst: u32) -> u32 {
        let esp = STACK_TOP - 0x200;
        session.memory.write_u32(esp, STOP).unwrap();
        session.memory.write_u32(esp + 4, SRC).unwrap();
        session.memory.write_u32(esp + 8, dst).unwrap();
        session.cpu.regs.esp = esp;
        session.cpu.regs.ecx = CTX;
        session.cpu.regs.eip = LZW_DECODE;
        esp
    }

    /// Runs the guest's own decoder, stepping the interpreter directly so the
    /// host routine's hook cannot answer for it.
    fn ran(session: &mut Session, dst: u32) -> (Vec<u8>, u32, u32, u32) {
        let esp = enter(session, dst);
        let mut steps = 0;
        loop {
            match crate::cpu::step(&mut session.memory, &mut session.cpu) {
                Ok(crate::cpu::Step::Continue) => steps += 1,
                Ok(crate::cpu::Step::Halt) => break,
                other => panic!("the guest decoder did not return: {other:?}"),
            }
            assert!(steps < 4_000_000, "the guest decoder did not return");
        }
        assert_eq!(session.cpu.regs.eip, STOP + 1, "the hlt was reached and run");
        assert_eq!(session.cpu.regs.esp, esp + 12, "stdcall pops its arguments");
        (
            session.memory.read_bytes(dst, 0x4000).unwrap().to_vec(),
            session.cpu.regs.eax,
            session.memory.read_u32(esp + 4).unwrap(),
            session.memory.read_u32(CTX.wrapping_add(0x30)).unwrap(),
        )
    }

    /// The host routine and the guest routine decode every stream to the same
    /// bytes, leave the same result, cursor and bit reservoir, and the guest's
    /// own routine costs far more instructions than the host's work estimate.
    fn differential(data: &[u8]) {
        let Some(mut session) = reference() else {
            return;
        };
        let stream = encode(data);
        prepare(&mut session, &stream);

        let cost_before = session.instructions();
        let (guest_out, guest_eax, guest_cursor, guest_count) = ran(&mut session, DST);
        let guest_cost = session.instructions() - cost_before;

        // The same call again, this time through the host routine: one step of
        // the run loop lands on the entry and the replacement finishes it.
        prepare(&mut session, &stream);
        let work_before = session.instructions();
        let esp = enter(&mut session, OTHER_DST);
        let outcome = session.run(1);
        assert_eq!(outcome, Outcome::Steps, "{outcome:?}");
        assert_eq!(session.cpu.regs.eip, STOP, "the host routine returned to the hlt");
        assert_eq!(session.cpu.regs.esp, esp + 12);
        let host_cost = session.instructions() - work_before;

        let host_out = session.memory.read_bytes(OTHER_DST, 0x4000).unwrap().to_vec();
        assert_eq!(host_out[..data.len()], guest_out[..data.len()], "decoded bytes");
        assert!(
            host_out[data.len()..].iter().all(|&byte| byte == 0),
            "nothing past the output was touched"
        );
        assert_eq!(session.cpu.regs.eax, guest_eax, "return value");
        assert_eq!(
            session.memory.read_u32(esp + 4).unwrap(),
            guest_cursor,
            "source cursor"
        );
        assert_eq!(
            session.memory.read_u32(CTX.wrapping_add(0x30)).unwrap(),
            guest_count,
            "bit reservoir count"
        );
        assert!(
            host_cost * 2 < guest_cost,
            "the host routine is not doing less work: {host_cost} vs {guest_cost}"
        );
    }

    #[test]
    fn a_single_literal_decodes() {
        differential(b"A");
    }

    #[test]
    fn repeated_words_match() {
        differential(b"TOBEORNOTTOBEORTOBEORNOT");
    }

    #[test]
    fn every_byte_value_and_alternation_match() {
        differential(&(0..=255_u8).cycle().take(1024).collect::<Vec<_>>());
    }

    #[test]
    fn pseudo_random_data_matches() {
        let mut state = 0x1234_5678_u32;
        let data: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
                (state >> 16) as u8
            })
            .collect();
        differential(&data);
    }

    #[test]
    fn a_long_run_of_one_byte_matches() {
        differential(&vec![b'Z'; 3000]);
    }

    // ---- sprite blitters ------------------------------------------------

    const SURFACE: u32 = 0x0120_0000;
    const SURFACE_PIXELS: u32 = 0x0121_0000;
    const SPRITE_PIXELS: u32 = 0x0122_0000;
    const SPRITE: u32 = 0x0123_0000;
    const POSITION: u32 = 0x0124_0000;
    const BLIT_LUT_DATA: u32 = 0x0125_0000;
    const BLIT_STOP: u32 = 0x0127_0000;
    const SURFACE_BYTES: usize = 0x1_0000;

    #[derive(Clone)]
    enum Segment {
        Literal(Vec<u8>),
        Skip(u16),
    }

    struct Case {
        name: &'static str,
        keyed: bool,
        width: u16,
        dest_x: i16,
        dest_y: u16,
        src_y: u16,
        clip_x: u16,
        clip_width: u16,
        rows: Vec<Vec<Segment>>,
    }

    /// Lays rows out the way the blitter reads them: a table of row offsets
    /// followed by each row's delta word and run stream.
    fn encode_sprite(rows: &[Vec<Segment>]) -> Vec<u8> {
        let table = rows.len() * 2;
        let mut offsets = Vec::new();
        let mut body = Vec::new();
        for row in rows {
            offsets.push(u16::try_from(table + body.len()).unwrap());
            let start = body.len();
            body.extend_from_slice(&[0, 0]);
            for segment in row {
                match segment {
                    Segment::Literal(bytes) => {
                        body.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
                        body.extend_from_slice(bytes);
                    }
                    Segment::Skip(count) => {
                        body.extend_from_slice(&(-(*count as i16)).to_le_bytes());
                    }
                }
            }
            let length = body.len() - start - 2;
            body[start..start + 2].copy_from_slice(&(length as u16).to_le_bytes());
        }
        let mut out = Vec::new();
        for offset in offsets {
            out.extend_from_slice(&offset.to_le_bytes());
        }
        out.extend_from_slice(&body);
        out
    }

    fn prepare_blit(session: &mut Session, case: &Case) {
        let sprite = encode_sprite(&case.rows);
        session
            .memory
            .write_bytes(SURFACE_PIXELS, &vec![0; SURFACE_BYTES])
            .unwrap();
        session.memory.write_bytes(SPRITE_PIXELS, &sprite).unwrap();

        session.memory.write_u16(SURFACE, 320).unwrap();
        session.memory.write_u32(SURFACE + 4, SURFACE_PIXELS).unwrap();
        session.memory.write_u16(SURFACE + 0xc, case.clip_x).unwrap();
        session.memory.write_u16(SURFACE + 0xe, 0).unwrap();
        session.memory.write_u16(SURFACE + 0x10, case.clip_width).unwrap();
        session.memory.write_u16(SURFACE + 0x12, 200).unwrap();

        session.memory.write_u16(SPRITE, case.width).unwrap();
        session.memory.write_u32(SPRITE + 4, SPRITE_PIXELS).unwrap();
        session.memory.write_u16(POSITION + 4, case.dest_x as u16).unwrap();

        session.memory.write_u16(BLIT_RECT + 2, case.src_y).unwrap();
        session.memory.write_u16(BLIT_RECT + 6, case.dest_y).unwrap();
        session
            .memory
            .write_u16(BLIT_RECT + 0xa, case.rows.len() as u16)
            .unwrap();

        let mut table = vec![0_u8; 0x1_0000];
        for source in 0..256usize {
            for under in 0..256usize {
                table[(source << 8) | under] = source
                    .wrapping_mul(167)
                    .wrapping_add(under.wrapping_mul(89))
                    .wrapping_add(13) as u8;
            }
        }
        session.memory.write_bytes(BLIT_LUT_DATA, &table).unwrap();
        session.memory.write_u32(BLIT_LUT, BLIT_LUT_DATA).unwrap();

        session.memory.write_u8(BLIT_STOP, 0xf4).unwrap();
    }

    fn enter_blit(session: &mut Session, case: &Case) -> u32 {
        let esp = STACK_TOP - 0x200;
        session.memory.write_u32(esp, BLIT_STOP).unwrap();
        session.cpu.regs.esp = esp;
        session.cpu.regs.eax = SPRITE;
        session.cpu.regs.edx = SURFACE;
        session.cpu.regs.ebx = POSITION;
        session.cpu.regs.ecx = BLIT_RECT;
        session.cpu.regs.eip = if case.keyed { BLIT_KEYED } else { BLIT_OPAQUE };
        esp
    }

    /// Runs the guest's own blitter and stops at the return.
    fn run_guest_blit(session: &mut Session, case: &Case) -> Vec<u8> {
        let esp = enter_blit(session, case);
        let mut steps = 0;
        while session.cpu.regs.eip != BLIT_STOP {
            match crate::cpu::step(&mut session.memory, &mut session.cpu) {
                Ok(crate::cpu::Step::Continue) => {}
                Ok(crate::cpu::Step::Halt) => break,
                other => panic!("the guest blitter did not return: {other:?}"),
            }
            steps += 1;
            assert!(steps < 8_000_000, "the guest blitter did not return");
        }
        assert_eq!(session.cpu.regs.eip, BLIT_STOP);
        assert_eq!(session.cpu.regs.esp, esp + 4, "the blitter pops its return");
        session
            .memory
            .read_bytes(SURFACE_PIXELS, SURFACE_BYTES)
            .unwrap()
            .to_vec()
    }

    /// Runs the host's replacement through the run loop's hook.
    fn run_host_blit(session: &mut Session, case: &Case) -> (Vec<u8>, u64) {
        let esp = enter_blit(session, case);
        let before = session.instructions();
        let outcome = session.run(1);
        assert_eq!(outcome, Outcome::Steps, "{}: {outcome:?}", case.name);
        assert_eq!(session.cpu.regs.eip, BLIT_STOP, "{}", case.name);
        assert_eq!(session.cpu.regs.esp, esp + 4, "{}", case.name);
        let work = session.instructions() - before;
        (
            session
                .memory
                .read_bytes(SURFACE_PIXELS, SURFACE_BYTES)
                .unwrap()
                .to_vec(),
            work,
        )
    }

    /// The guest and the host must leave every surface byte identical, and the
    /// host's instruction estimate must stay below what the guest retired.
    fn differential_blits(cases: Vec<Case>) {
        let Some(mut session) = reference() else {
            return;
        };
        for case in cases {
            prepare_blit(&mut session, &case);
            let before = session.instructions();
            let guest = run_guest_blit(&mut session, &case);
            let guest_cost = session.instructions() - before;

            prepare_blit(&mut session, &case);
            let (host, work) = run_host_blit(&mut session, &case);

            assert_eq!(host, guest, "{} surface bytes", case.name);
            assert!(
                work < guest_cost,
                "{}: the host estimate {work} is not below the guest's {guest_cost}",
                case.name
            );
        }
    }

    fn one_row(width: u16, dest_x: i16, segments: Vec<Segment>) -> Case {
        Case {
            name: "one row",
            keyed: false,
            width,
            dest_x,
            dest_y: 0,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![segments],
        }
    }

    #[test]
    fn literal_runs_and_skips_match_in_both_blitters() {
        for keyed in [false, true] {
            let mut case = one_row(
                8,
                0,
                vec![
                    Segment::Literal(vec![1, 2, 3, 4]),
                    Segment::Skip(2),
                    Segment::Literal(vec![9, 8]),
                ],
            );
            case.keyed = keyed;
            case.name = if keyed { "keyed literal row" } else { "opaque literal row" };
            differential_blits(vec![case]);
        }
    }

    #[test]
    fn runs_clip_against_the_destination_rectangle() {
        let mut cases = Vec::new();
        // Starts left of the clip and loses three source pixels.
        cases.push(Case {
            name: "left clip",
            keyed: false,
            width: 8,
            dest_x: -3,
            dest_y: 0,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8])]],
        });
        // Starts near the right edge; the run is cut to four pixels.
        cases.push(Case {
            name: "right cut",
            keyed: false,
            width: 8,
            dest_x: 316,
            dest_y: 0,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![
                vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8])],
                vec![Segment::Literal(vec![11, 12, 13, 14, 15, 16, 17, 18])],
            ],
        });
        // Left-clipped and still past the right edge: the guest restores the
        // unclipped start for its bookkeeping, which the host mirrors.
        cases.push(Case {
            name: "left clip and right overflow",
            keyed: false,
            width: 400,
            dest_x: -10,
            dest_y: 1,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![vec![Segment::Literal((0..400).map(|v| v as u8).collect())]],
        });
        // Entirely left and entirely right of the clip.
        cases.push(Case {
            name: "wholly left",
            keyed: false,
            width: 8,
            dest_x: -20,
            dest_y: 0,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8])]],
        });
        cases.push(Case {
            name: "wholly right",
            keyed: false,
            width: 8,
            dest_x: 400,
            dest_y: 0,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8])]],
        });
        // A non-zero left edge moves the whole picture.
        cases.push(Case {
            name: "shifted clip",
            keyed: false,
            width: 10,
            dest_x: 30,
            dest_y: 2,
            src_y: 0,
            clip_x: 40,
            clip_width: 200,
            rows: vec![vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10])]],
        });
        // The same geometry through the palette path.
        cases.push(Case {
            name: "keyed right cut",
            keyed: true,
            width: 8,
            dest_x: 316,
            dest_y: 0,
            src_y: 0,
            clip_x: 0,
            clip_width: 320,
            rows: vec![
                vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8])],
                vec![Segment::Literal(vec![11, 12, 13, 14, 15, 16, 17, 18])],
            ],
        });
        differential_blits(cases);
    }

    #[test]
    fn a_row_ends_when_the_sprite_width_is_consumed() {
        let mut cases = Vec::new();
        // Two skips in a row merge, then the literal fills the width.
        cases.push(one_row(
            10,
            0,
            vec![
                Segment::Skip(3),
                Segment::Skip(2),
                Segment::Literal(vec![4, 5, 6, 7, 8]),
            ],
        ));
        // A skip past the width ends the row with nothing drawn.
        let mut case = one_row(10, 0, vec![Segment::Skip(20)]);
        case.name = "skip overshoot";
        cases.push(case);
        // A literal longer than the width is not trimmed to it; both
        // implementations paint the whole run.
        let mut case = one_row(5, 0, vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6, 7, 8])]);
        case.name = "run past the width";
        cases.push(case);
        differential_blits(cases);
    }

    #[test]
    fn the_row_table_selects_the_first_source_row() {
        let mut case = one_row(6, 10, vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6])]);
        case.name = "second source row";
        case.src_y = 1;
        case.rows = vec![
            vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6])],
            vec![Segment::Literal(vec![21, 22, 23, 24, 25, 26])],
        ];
        case.dest_y = 5;
        differential_blits(vec![case]);
    }

    #[test]
    fn rows_advance_by_their_delta_and_pitch() {
        let mut case = one_row(6, 20, vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6])]);
        case.name = "three rows";
        case.rows = vec![
            vec![Segment::Literal(vec![1, 2, 3, 4, 5, 6])],
            vec![Segment::Literal(vec![11, 12, 13, 14, 15, 16])],
            vec![Segment::Literal(vec![21, 22, 23, 24, 25, 26])],
        ];
        case.dest_y = 3;
        differential_blits(vec![case]);
    }
}

