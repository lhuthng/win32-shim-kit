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

use crate::cpu::{Cpu, Flags};
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

// ---- Hercules' software sprite rows ---------------------------------------
//
// Hercules draws every frame itself into one 320x240 8-bit DIB and hands it to
// StretchDIBits. Three inner row loops are about 60% of all gameplay
// instructions. Each is a pure memory loop -- no imports, no calls, no stack
// use -- so the host can retire the whole row natively and resume at the first
// instruction after the loop. A branch scan of .text shows each entry address
// is reached only by fall-through from its own row setup and by its own back
// edge, so there is no caller state to reconstruct.
//
// These are the exact loops where the partial-register sprite bug lived. The
// replacements below are transcribed instruction by instruction, including the
// arithmetic flags, and are checked against the interpreter in the tests at
// the bottom of this file.

/// `0x0040cbf4`: `mov al, [esi+ebx]; test al,al; je skip; mov [edi+ebx],al`.
pub const HERCULES_KEYED_ROW: u32 = 0x0040_cbf4;
/// `0x0040ca51`: the same row through a 256x256 `table[source << 8 | dest]`.
pub const HERCULES_REMAP_ROW: u32 = 0x0040_ca51;
/// `0x0040e56c`: the fixed-point scaled row, with `adc` stepping and `dec ax`.
pub const HERCULES_SCALED_ROW: u32 = 0x0040_e56c;

/// First instruction after each loop. The row setup and the outer row loop
/// stay interpreted; only the hot inner loop is replaced.
const HERCULES_KEYED_EXIT: u32 = 0x0040_cc01;
const HERCULES_REMAP_EXIT: u32 = 0x0040_ca64;
const HERCULES_SCALED_EXIT: u32 = 0x0040_e5a8;

/// Fixed-point steps and the shade/remap tables the scaled row reads. They are
/// written before the loop and never change inside it.
const HERCULES_Y_STEP: u32 = 0x00a9_0998;
const HERCULES_X_STEP: u32 = 0x00a9_099c;
const HERCULES_SCALE_LUT: u32 = 0x00a9_09a0;

/// The arithmetic flags after one instruction. `df` is untouched by every
/// instruction in these loops, so it is carried through rather than modelled.
struct RowFlags {
    cf: bool,
    pf: bool,
    af: bool,
    zf: bool,
    sf: bool,
    of: bool,
}

#[derive(Clone, Copy)]
enum RowAlu {
    Add,
    Sub,
}

/// A byte/word/dword ALU operation, matching `cpu::exec::apply_alu` exactly.
/// `size` is `(mask, sign)`; the dword pair is `(0xffff_ffff, 0x8000_0000)`.
fn row_alu(
    operation: RowAlu,
    a: u32,
    b: u32,
    carry_in: bool,
    size: (u32, u32),
) -> (u32, RowFlags) {
    let (mask, sign) = size;
    let a = a & mask;
    let b = b & mask;
    let (result, carry, overflow, adjust) = match operation {
        RowAlu::Add => {
            let wide = u64::from(a) + u64::from(b) + u64::from(carry_in);
            let result = (wide as u32) & mask;
            (
                result,
                wide > u64::from(mask),
                ((a ^ result) & (b ^ result) & sign) != 0,
                ((a ^ b ^ result) & 0x10) != 0,
            )
        }
        RowAlu::Sub => {
            let lhs = u64::from(a);
            let rhs = u64::from(b) + u64::from(carry_in);
            let result = (lhs.wrapping_sub(rhs) as u32) & mask;
            (
                result,
                lhs < rhs,
                ((a ^ b) & (a ^ result) & sign) != 0,
                ((a ^ b ^ result) & 0x10) != 0,
            )
        }
    };
    (
        result,
        RowFlags {
            cf: carry,
            pf: Flags::parity(result),
            af: adjust,
            zf: result == 0,
            sf: result & sign != 0,
            of: overflow,
        },
    )
}

const ROW_WORD: (u32, u32) = (0x0000_ffff, 0x0000_8000);
const ROW_DWORD: (u32, u32) = (0xffff_ffff, 0x8000_0000);

fn row_set_flags(cpu: &mut Cpu, flags: RowFlags) {
    cpu.regs.flags.cf = flags.cf;
    cpu.regs.flags.pf = flags.pf;
    cpu.regs.flags.af = flags.af;
    cpu.regs.flags.zf = flags.zf;
    cpu.regs.flags.sf = flags.sf;
    cpu.regs.flags.of = flags.of;
}

/// The `test` inside all three row loops feeds nothing but its own conditional
/// jump. The `inc`/`dec` that closes the same iteration overwrites every flag
/// except the carry, and `inc`/`dec` leave the carry alone, so the only flag
/// that outlives the iteration is `cf` -- which `test` clears. Computing parity
/// and storing five flags per pixel bought nothing; the equivalence tests still
/// compare every flag after the whole row.
#[inline(always)]
fn row_clear_carry(cpu: &mut Cpu) {
    cpu.regs.flags.cf = false;
}

/// `inc`/`dec` are `add`/`sub` with the carry flag left alone.
fn row_inc_dec(cpu: &mut Cpu, value: u32, size: (u32, u32), down: bool) -> u32 {
    let carry = cpu.regs.flags.cf;
    let (result, mut flags) = row_alu(
        if down { RowAlu::Sub } else { RowAlu::Add },
        value,
        1,
        false,
        size,
    );
    flags.cf = carry;
    row_set_flags(cpu, flags);
    result
}

fn row_byte(store: &[u8], va: u32, what: &str) -> Result<u8> {
    let at = guest_offset(store, va, 1, what)?;
    Ok(store[at])
}

fn row_store_byte(store: &mut [u8], va: u32, value: u8, what: &str) -> Result<()> {
    let at = guest_offset(store, va, 1, what)?;
    store[at] = value;
    Ok(())
}

/// Counters, so an embedder can prove the replacements actually ran.
#[derive(Default)]
pub struct HerculesBlitter {
    calls: u64,
    pixels: u64,
    work: u64,
}

impl HerculesBlitter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn pixels(&self) -> u64 {
        self.pixels
    }

    /// Guest instructions these rows stood in for. The step loop adds it to the
    /// instruction counter, so it is also the interpreter work that was skipped.
    pub fn work(&self) -> u64 {
        self.work
    }

    /// `0x40cbf4..0x40cc00`. `ebx` is `-width` on entry and counts up to zero;
    /// the loop is bottom-tested, so even a zero width runs one iteration.
    ///
    /// Only the *last* `inc ebx` leaves a flag for the guest: every earlier one
    /// is overwritten by the next iteration's `test`. So the loop runs flag-free
    /// and the final step is the only one that pays for `row_inc_dec`. The carry
    /// is cleared once up front because `test` clears it on every iteration and
    /// `inc` preserves it.
    pub fn keyed_row(&mut self, memory: &mut GuestMemory, cpu: &mut Cpu) -> Result<u32> {
        let store = memory.as_mut_slice();
        let mut work = 0_u32;
        let mut drawn = 0_u64;
        row_clear_carry(cpu);
        loop {
            let index = cpu.regs.ebx;
            let pixel = row_byte(store, cpu.regs.esi.wrapping_add(index), "keyed source")?;
            // mov al, [esi+ebx]
            cpu.regs.eax = (cpu.regs.eax & !0xff) | u32::from(pixel);
            // test al, al / je (the jump itself is one instruction either way)
            work += 3;
            if pixel != 0 {
                row_store_byte(
                    store,
                    cpu.regs.edi.wrapping_add(index),
                    pixel,
                    "keyed destination",
                )?;
                drawn += 1;
                work += 1;
            }
            // inc ebx / jl
            let next = index.wrapping_add(1);
            work += 2;
            if (next as i32) >= 0 {
                cpu.regs.ebx = row_inc_dec(cpu, index, ROW_DWORD, false);
                break;
            }
            cpu.regs.ebx = next;
        }
        cpu.regs.eip = HERCULES_KEYED_EXIT;
        self.calls += 1;
        self.pixels += drawn;
        self.work += u64::from(work);
        Ok(work)
    }

    /// `0x40ca51..0x40ca63`. `ah` is the source pixel, `al` the pixel already
    /// on the destination, and `[ecx+eax]` the 256x256 shade/remap table.
    pub fn remap_row(&mut self, memory: &mut GuestMemory, cpu: &mut Cpu) -> Result<u32> {
        let store = memory.as_mut_slice();
        let mut work = 0_u32;
        let mut drawn = 0_u64;
        // Same shape as the keyed row: the `test` clears the carry, `inc ebx`
        // preserves it, and every flag but the carry is overwritten before the
        // loop can exit, so only the last `inc` needs the full computation.
        row_clear_carry(cpu);
        loop {
            let index = cpu.regs.ebx;
            let destination = cpu.regs.edi.wrapping_add(index);
            let pixel = row_byte(store, cpu.regs.esi.wrapping_add(index), "remap source")?;
            // mov ah, [esi+ebx] / test ah, ah / je
            cpu.regs.eax = (cpu.regs.eax & !0xff00) | (u32::from(pixel) << 8);
            work += 3;
            if pixel != 0 {
                // mov al, [edi+ebx]
                let under = row_byte(store, destination, "remap destination")?;
                cpu.regs.eax = (cpu.regs.eax & !0xff) | u32::from(under);
                // mov al, [ecx+eax]
                let mapped = row_byte(
                    store,
                    cpu.regs.ecx.wrapping_add(cpu.regs.eax),
                    "remap table",
                )?;
                cpu.regs.eax = (cpu.regs.eax & !0xff) | u32::from(mapped);
                // mov [edi+ebx], al
                row_store_byte(store, destination, mapped, "remap destination")?;
                drawn += 1;
                work += 3;
            }
            let next = index.wrapping_add(1);
            work += 2;
            if (next as i32) >= 0 {
                cpu.regs.ebx = row_inc_dec(cpu, index, ROW_DWORD, false);
                break;
            }
            cpu.regs.ebx = next;
        }
        cpu.regs.eip = HERCULES_REMAP_EXIT;
        self.calls += 1;
        self.pixels += drawn;
        self.work += u64::from(work);
        Ok(work)
    }

    /// `0x40e56c..0x40e5a7`. This is the loop whose `dec ax` must leave the
    /// upper half of `eax` alone: that half holds a fixed-point source
    /// coordinate. Every `adc` below consumes the carry of the instruction
    /// immediately before it, so the native walk keeps the same carry chain.
    pub fn scaled_row(&mut self, memory: &mut GuestMemory, cpu: &mut Cpu) -> Result<u32> {
        let y_step = memory.read_u32(HERCULES_Y_STEP)?;
        let x_step = memory.read_u32(HERCULES_X_STEP)?;
        let scale_lut = memory.read_u32(HERCULES_SCALE_LUT)?;
        let store = memory.as_mut_slice();
        let mut work = 0_u32;
        let mut drawn = 0_u64;
        loop {
            // mov dh, bl
            cpu.regs.edx = (cpu.regs.edx & !0xff00) | ((cpu.regs.ebx & 0xff) << 8);
            work += 1;
            // add eax, [y step]
            let (result, flags) = row_alu(RowAlu::Add, cpu.regs.eax, y_step, false, ROW_DWORD);
            row_set_flags(cpu, flags);
            cpu.regs.eax = result;
            work += 1;
            // adc ebx, ecx
            let carry = cpu.regs.flags.cf;
            let (result, flags) =
                row_alu(RowAlu::Add, cpu.regs.ebx, cpu.regs.ecx, carry, ROW_DWORD);
            row_set_flags(cpu, flags);
            cpu.regs.ebx = result;
            work += 1;
            // mov dl, 0
            cpu.regs.edx &= !0xff;
            work += 1;
            // mov dh, [esi+edx]
            let pixel = row_byte(store, cpu.regs.esi.wrapping_add(cpu.regs.edx), "scaled source")?;
            cpu.regs.edx = (cpu.regs.edx & !0xff00) | (u32::from(pixel) << 8);
            work += 1;
            // adc esi, [x step] -- the mov instructions above do not touch flags
            let carry = cpu.regs.flags.cf;
            let (result, flags) =
                row_alu(RowAlu::Add, cpu.regs.esi, x_step, carry, ROW_DWORD);
            row_set_flags(cpu, flags);
            cpu.regs.esi = result;
            work += 1;
            // test dh, dh / je
            row_clear_carry(cpu);
            work += 2;
            if pixel != 0 {
                // mov dl, dh
                cpu.regs.edx = (cpu.regs.edx & !0xff) | u32::from(pixel);
                work += 1;
                // and edx, 0xff -- flags die here: `add` below overwrites them
                // and the `and edx, 0xffff` after it clears the carry, which is
                // the only flag `inc edi`/`dec ax` would carry out of the row.
                cpu.regs.edx &= 0xff;
                work += 1;
                // add edx, [shade table] -- flags die at the `and` below
                cpu.regs.edx = cpu.regs.edx.wrapping_add(scale_lut);
                work += 1;
                // mov dh, [edx]
                let shade = row_byte(store, cpu.regs.edx, "scaled shade")?;
                cpu.regs.edx = (cpu.regs.edx & !0xff00) | (u32::from(shade) << 8);
                work += 1;
                // and edx, 0xffff
                cpu.regs.edx &= 0xffff;
                work += 1;
                // mov dl, [edi]
                let under = row_byte(store, cpu.regs.edi, "scaled destination")?;
                cpu.regs.edx = (cpu.regs.edx & !0xff) | u32::from(under);
                work += 1;
                // mov dl, [ebp+edx]
                let mapped = row_byte(
                    store,
                    cpu.regs.ebp.wrapping_add(cpu.regs.edx),
                    "scaled remap",
                )?;
                cpu.regs.edx = (cpu.regs.edx & !0xff) | u32::from(mapped);
                work += 1;
                // mov [edi], dl
                row_store_byte(store, cpu.regs.edi, mapped, "scaled destination")?;
                drawn += 1;
                work += 1;
            }
            // inc edi -- `dec ax` overwrites every flag it sets, and the carry
            // is already clear, so this is a plain increment.
            cpu.regs.edi = cpu.regs.edi.wrapping_add(1);
            work += 1;
            // dec ax -- a 16-bit write, so the upper half survives
            let low = row_inc_dec(cpu, cpu.regs.eax & 0xffff, ROW_WORD, true);
            cpu.regs.eax = (cpu.regs.eax & !0xffff) | (low & 0xffff);
            work += 1;
            // jg: taken while !zf && sf == of
            work += 1;
            if cpu.regs.flags.zf || cpu.regs.flags.sf != cpu.regs.flags.of {
                break;
            }
        }
        cpu.regs.eip = HERCULES_SCALED_EXIT;
        self.calls += 1;
        self.pixels += drawn;
        self.work += u64::from(work);
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

/// Madagascar's hand-written FPU vertex transform at `0x005c92a0`, in the
/// image's custom `_rwcseg` ("read-write code") section. The game registers the
/// address with a dispatcher at startup (`0x4e6860` binds it after setting
/// MXCSR flush-to-zero), and every call that follows goes through that
/// dispatcher, which is why no direct call site names it. Two instruction
/// samples 19 billion instructions apart both landed inside this one function:
/// it is the largest single consumer of guest instructions during the boot.
///
/// The contract, from the kernel's own unrolled body: `mat_transform(dst, src,
/// matrix)` with three stack arguments the caller cleans up (the kernel ends
/// in a plain `ret`). `src` holds **four** vertices in 16-byte slots, `dst`
/// receives four in the same layout, and `matrix` is a column-major 4x4 whose
/// element (row, column) lives at `matrix + column * 16 + row * 4`. Each
/// destination vertex gets `x' = m00*x + m01*y + m02*z`, the same for `y` and
/// `z` through the third column -- and the fourth vertex alone adds the
/// translation column (`m03`, `m13`, `m23`), which is the shape of three edge
/// vectors transformed around one absolute position. The fourth slot of each
/// 16-byte vertex is neither read nor written.
///
/// The guest computes in x87 registers, which carry every product of two f32s
/// exactly and round once, on the store. The host keeps its partial sums in
/// f64 -- exact for the same products -- and rounds at the same place, so the
/// values left in guest memory are the values the FPU left.
pub const MAT_TRANSFORM: u32 = 0x005c_92a0;

/// The unrolled kernel spends its ~153 instructions on four vertices, so that
/// is what one native call stands for in every budget and trace.
const MAT_TRANSFORM_COST: u32 = 153;

pub struct MatTransform {
    calls: u64,
    vertices: u64,
}

impl MatTransform {
    pub fn new() -> Self {
        Self {
            calls: 0,
            vertices: 0,
        }
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn vertices(&self) -> u64 {
        self.vertices
    }

    pub fn apply(&mut self, memory: &mut GuestMemory, cpu: &mut Cpu) -> Result<u32> {
        let esp = cpu.regs.esp;
        let return_address = memory.read_u32(esp)?;
        let dst_va = memory.read_u32(esp.wrapping_add(4))?;
        let src_va = memory.read_u32(esp.wrapping_add(8))?;
        let matrix_va = memory.read_u32(esp.wrapping_add(12))?;

        // m[column * 4 + row]: the sixteen entries a column-major 4x4 holds,
        // of which the kernel reads twelve for rotation and three for the
        // translation it adds to the fourth vertex.
        let mut m = [0.0_f32; 16];
        for (index, entry) in m.iter_mut().enumerate() {
            *entry = f32::from_bits(memory.read_u32(matrix_va.wrapping_add(index as u32 * 4))?);
        }

        for vertex in 0..4_u32 {
            let at = src_va.wrapping_add(vertex * 16);
            let x = f64::from(f32::from_bits(memory.read_u32(at)?));
            let y = f64::from(f32::from_bits(memory.read_u32(at.wrapping_add(4))?));
            let z = f64::from(f32::from_bits(memory.read_u32(at.wrapping_add(8))?));
            // The translation column joins in only for the fourth vertex.
            let translate = vertex == 3;
            for row in 0..3_usize {
                let mut sum = f64::from(m[row]) * x
                    + f64::from(m[4 + row]) * y
                    + f64::from(m[8 + row]) * z;
                if translate {
                    sum += f64::from(m[12 + row]);
                }
                memory.write_u32(
                    dst_va.wrapping_add(vertex * 16 + row as u32 * 4),
                    (sum as f32).to_bits(),
                )?;
            }
        }

        self.calls += 1;
        self.vertices += 4;
        cpu.regs.eip = return_address;
        // A plain `ret`: the return address is popped, the caller cleans its
        // own twelve argument bytes.
        cpu.regs.esp = esp.wrapping_add(4);
        Ok(MAT_TRANSFORM_COST)
    }
}

/// The reference image's replacements, which [`crate::session::Session::load`]
/// arms: a session built without a target of its own is a session against the
/// image these addresses belong to.
pub static REFERENCE_HLE: &[crate::target::HleEntry] = &[
    crate::target::HleEntry {
        address: LZW_DECODE,
        kind: crate::target::Hle::Lzw,
    },
    crate::target::HleEntry {
        address: BLIT_OPAQUE,
        kind: crate::target::Hle::Blit { keyed: false },
    },
    crate::target::HleEntry {
        address: BLIT_KEYED,
        kind: crate::target::Hle::Blit { keyed: true },
    },
];

/// Madagascar's replacement, armed for a payload whose program is `Game.exe`.
pub static MADAGASCAR_HLE: &[crate::target::HleEntry] = &[crate::target::HleEntry {
    address: MAT_TRANSFORM,
    kind: crate::target::Hle::MatTransform,
}];

/// Hercules' three sprite rows, armed only for a `HERCULES.EXE` payload. These
/// addresses are code in that image and data in others, so the program name is
/// what keeps them from answering for the wrong game.
pub static HERCULES_HLE: &[crate::target::HleEntry] = &[
    crate::target::HleEntry {
        address: HERCULES_KEYED_ROW,
        kind: crate::target::Hle::HerculesKeyedRow,
    },
    crate::target::HleEntry {
        address: HERCULES_REMAP_ROW,
        kind: crate::target::Hle::HerculesRemapRow,
    },
    crate::target::HleEntry {
        address: HERCULES_SCALED_ROW,
        kind: crate::target::Hle::HerculesScaledRow,
    },
];

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
            match crate::cpu::step(&mut session.memory, &mut session.cpu, &mut session.decodes) {
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
            match crate::cpu::step(&mut session.memory, &mut session.cpu, &mut session.decodes) {
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

    // ---- Madagascar's vertex transform ------------------------------------

    const MAT_STOP: u32 = 0x0117_0000;
    const MAT_DST: u32 = 0x0117_1000;
    const MAT_SRC: u32 = 0x0117_2000;
    const MAT_MATRIX: u32 = 0x0117_3000;

    fn mat_memory() -> GuestMemory {
        let memory = GuestMemory::empty(crate::memory::HEAP_BASE as usize + 0x1000)
            .expect("blank memory");
        memory
    }

    /// Column-major 4x4: element (row, column) at `matrix + column * 16 + row * 4`.
    fn write_matrix(memory: &mut GuestMemory, m: [f32; 16]) {
        for (index, value) in m.iter().enumerate() {
            memory
                .write_u32(MAT_MATRIX + index as u32 * 4, value.to_bits())
                .unwrap();
        }
    }

    /// Four vertices in 16-byte slots: x, y, z, and an untouched fourth slot.
    fn write_vertices(memory: &mut GuestMemory, vertices: &[[f32; 3]; 4]) {
        for (index, vertex) in vertices.iter().enumerate() {
            for (row, value) in vertex.iter().enumerate() {
                memory
                    .write_u32(
                        MAT_SRC + index as u32 * 16 + row as u32 * 4,
                        value.to_bits(),
                    )
                    .unwrap();
            }
            memory
                .write_u32(MAT_SRC + index as u32 * 16 + 12, 0xdead_beef)
                .unwrap();
        }
        for index in 0..4_u32 {
            for row in 0..4_u32 {
                memory
                    .write_u32(MAT_DST + index * 16 + row * 4, 0xfeed_face)
                    .unwrap();
            }
        }
    }

    fn read_vertex(memory: &GuestMemory, index: u32) -> [f32; 3] {
        let mut out = [0.0_f32; 3];
        for (row, slot) in out.iter_mut().enumerate() {
            *slot =
                f32::from_bits(memory.read_u32(MAT_DST + index * 16 + row as u32 * 4).unwrap());
        }
        out
    }

    fn mat_stack(memory: &mut GuestMemory) -> u32 {
        let esp = STACK_TOP - 0x400;
        memory.write_u32(esp, MAT_STOP).unwrap();
        memory.write_u32(esp + 4, MAT_DST).unwrap();
        memory.write_u32(esp + 8, MAT_SRC).unwrap();
        memory.write_u32(esp + 12, MAT_MATRIX).unwrap();
        esp
    }

    /// Identity rotation, translation (10, 20, 30): the first three vertices
    /// pass through untouched, and the fourth alone is translated.
    #[test]
    fn the_first_three_vertices_skip_translation_and_the_fourth_gets_it() {
        let mut memory = mat_memory();
        write_matrix(
            &mut memory,
            [
                1.0, 0.0, 0.0, 0.0, // column 0
                0.0, 1.0, 0.0, 0.0, // column 1
                0.0, 0.0, 1.0, 0.0, // column 2
                10.0, 20.0, 30.0, 0.0, // column 3, the translation
            ],
        );
        write_vertices(
            &mut memory,
            &[[1.0, 2.0, 3.0], [4.0, 5.0, 6.0], [7.0, 8.0, 9.0], [2.0, 4.0, 6.0]],
        );
        let esp = mat_stack(&mut memory);
        let mut cpu = Cpu {
            regs: crate::cpu::Registers {
                esp,
                eip: MAT_TRANSFORM,
                ..crate::cpu::Registers::default()
            },
            ..Default::default()
        };
        let mut transform = MatTransform::new();

        let work = transform.apply(&mut memory, &mut cpu).unwrap();

        assert_eq!(read_vertex(&memory, 0), [1.0, 2.0, 3.0]);
        assert_eq!(read_vertex(&memory, 1), [4.0, 5.0, 6.0]);
        assert_eq!(read_vertex(&memory, 2), [7.0, 8.0, 9.0]);
        assert_eq!(read_vertex(&memory, 3), [12.0, 24.0, 36.0]);
        // The fourth slot of each vertex is neither read nor written.
        assert_eq!(
            memory.read_u32(MAT_DST + 12).unwrap(),
            0xfeed_face,
            "the slot after vertex 0's z"
        );
        // The stack contract: a plain `ret`, so only the return address goes.
        assert_eq!(cpu.regs.eip, MAT_STOP);
        assert_eq!(cpu.regs.esp, esp + 4);
        assert_eq!(work, MAT_TRANSFORM_COST);
        assert_eq!(transform.calls(), 1);
        assert_eq!(transform.vertices(), 4);
    }

    /// Scale with translation, against values a hand computation gets exactly.
    #[test]
    fn the_matrix_math_is_the_columns_the_kernel_reads() {
        let mut memory = mat_memory();
        // x' = 2x + 100, y' = 3y + 200, z' = 0.5z + 50 for the fourth vertex;
        // the first three get the scale alone.
        write_matrix(
            &mut memory,
            [
                2.0, 0.0, 0.0, 0.0, //
                0.0, 3.0, 0.0, 0.0, //
                0.0, 0.0, 0.5, 0.0, //
                100.0, 200.0, 50.0, 0.0, //
            ],
        );
        write_vertices(
            &mut memory,
            &[[1.0, 1.0, 1.0], [0.0, 0.0, 0.0], [3.0, 2.0, 8.0], [1.0, 1.0, 1.0]],
        );
        let esp = mat_stack(&mut memory);
        let mut cpu = Cpu {
            regs: crate::cpu::Registers {
                esp,
                eip: MAT_TRANSFORM,
                ..crate::cpu::Registers::default()
            },
            ..Default::default()
        };
        let mut transform = MatTransform::new();
        transform.apply(&mut memory, &mut cpu).unwrap();

        assert_eq!(read_vertex(&memory, 0), [2.0, 3.0, 0.5]);
        assert_eq!(read_vertex(&memory, 1), [0.0, 0.0, 0.0]);
        assert_eq!(read_vertex(&memory, 2), [6.0, 6.0, 4.0]);
        assert_eq!(read_vertex(&memory, 3), [102.0, 203.0, 50.5]);
    }

    #[test]
    fn a_vertex_outside_guest_memory_is_an_error_not_a_write() {
        let mut memory = mat_memory();
        write_matrix(&mut memory, [0.0; 16]);
        write_vertices(&mut memory, &[[0.0; 3]; 4]);
        let esp = mat_stack(&mut memory);
        memory
            .write_u32(esp + 8, MAT_SRC + 0x0100_0000)
            .unwrap();
        let mut cpu = Cpu {
            regs: crate::cpu::Registers {
                esp,
                eip: MAT_TRANSFORM,
                ..crate::cpu::Registers::default()
            },
            ..Default::default()
        };
        let mut transform = MatTransform::new();
        assert!(transform.apply(&mut memory, &mut cpu).is_err());
        assert_eq!(
            memory.read_u32(MAT_DST).unwrap(),
            0xfeed_face,
            "nothing was written on the way out"
        );
    }

    // ---- Hercules row equivalence -----------------------------------------
    //
    // Each test boots the *real* loop bytes through the interpreter, then boots
    // the same state through the native replacement, and compares every
    // register, every flag and every byte of guest memory. A mismatch anywhere
    // -- including the instruction count the replacement claims -- means the
    // replacement cannot be shipped. This is the A/B the sprite fix demanded.

    const ROW_MEMORY: usize = 0x0100_0000;
    const ROW_SRC: u32 = 0x0110_0000;
    const ROW_DST: u32 = 0x0112_0000;
    const ROW_LUT: u32 = 0x0114_0000;
    const ROW_SHADE: u32 = 0x0115_0000;
    const ROW_REMAP: u32 = 0x0116_0000;
    const ROW_STACK: u32 = 0x0118_0000;

    /// Copied out of `HERCULES.EXE`'s .text, so both sides execute the same
    /// bytes the game does.
    const KEYED_ROW_BYTES: &[u8] = &[
        0x8a, 0x04, 0x1e, 0x84, 0xc0, 0x74, 0x03, 0x88, 0x04, 0x1f, 0x43, 0x7c, 0xf3,
    ];
    const REMAP_ROW_BYTES: &[u8] = &[
        0x8a, 0x24, 0x1e, 0x84, 0xe4, 0x74, 0x09, 0x8a, 0x04, 0x1f, 0x8a, 0x04, 0x01,
        0x88, 0x04, 0x1f, 0x43, 0x7c, 0xed,
    ];
    const SCALED_ROW_BYTES: &[u8] = &[
        0x8a, 0xf3, 0x03, 0x05, 0x98, 0x09, 0xa9, 0x00, 0x13, 0xd9, 0xb2, 0x00, 0x8a,
        0x34, 0x16, 0x13, 0x35, 0x9c, 0x09, 0xa9, 0x00, 0x84, 0xf6, 0x74, 0x1e, 0x8a,
        0xd6, 0x81, 0xe2, 0xff, 0x00, 0x00, 0x00, 0x03, 0x15, 0xa0, 0x09, 0xa9, 0x00,
        0x8a, 0x32, 0x81, 0xe2, 0xff, 0xff, 0x00, 0x00, 0x8a, 0x17, 0x8a, 0x54, 0x15,
        0x00, 0x88, 0x17, 0x47, 0x66, 0x48, 0x7f, 0xc4,
    ];

    fn fill_row(store: &mut [u8], va: u32, length: usize, seed: u32) {
        let at = (va - IMAGE_BASE) as usize;
        let mut state = seed;
        for slot in &mut store[at..at + length] {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            // Roughly one byte in eight is zero, so both the transparent and
            // the painted path are exercised.
            *slot = if state >> 28 == 0 { 0 } else { (state >> 24) as u8 };
        }
    }

    fn row_machine(code_va: u32, code: &[u8]) -> (GuestMemory, Cpu) {
        let mut memory = GuestMemory::empty(ROW_MEMORY).unwrap();
        {
            let store = memory.as_mut_slice();
            let at = (code_va - IMAGE_BASE) as usize;
            store[at..at + code.len()].copy_from_slice(code);
            fill_row(store, ROW_SRC, 0x2_0000, 0x1234_5678);
            fill_row(store, ROW_DST, 0x2_0000, 0x8765_4321);
            fill_row(store, ROW_LUT, 0x1_0000, 0xa5a5_5a5a);
            fill_row(store, ROW_SHADE, 0x100, 0x0f0f_f0f0);
            fill_row(store, ROW_REMAP, 0x1_0000, 0xdead_beef);
        }
        let mut cpu = Cpu::at(code_va);
        cpu.regs.esp = ROW_STACK;
        cpu.regs.flags = Flags {
            cf: true,
            pf: false,
            af: true,
            zf: false,
            sf: true,
            of: false,
            df: false,
        };
        (memory, cpu)
    }

    fn interpret_row(memory: &mut GuestMemory, cpu: &mut Cpu, exit: u32) -> u64 {
        let mut decodes = crate::cpu::Decodes::new();
        let mut steps = 0_u64;
        while cpu.regs.eip != exit {
            crate::cpu::step(memory, cpu, &mut decodes).unwrap();
            steps += 1;
            assert!(steps < 100_000, "the row never reached {exit:#010x}");
        }
        assert_eq!(cpu.instructions, steps);
        steps
    }

    fn assert_row_equivalent(
        label: &str,
        reference: &Cpu,
        native: &Cpu,
        ref_memory: &GuestMemory,
        native_memory: &GuestMemory,
        steps: u64,
        work: u32,
    ) {
        assert_eq!(u64::from(work), steps, "{label}: instruction accounting");
        assert_eq!(native.regs.eax, reference.regs.eax, "{label}: eax");
        assert_eq!(native.regs.ecx, reference.regs.ecx, "{label}: ecx");
        assert_eq!(native.regs.edx, reference.regs.edx, "{label}: edx");
        assert_eq!(native.regs.ebx, reference.regs.ebx, "{label}: ebx");
        assert_eq!(native.regs.esp, reference.regs.esp, "{label}: esp");
        assert_eq!(native.regs.ebp, reference.regs.ebp, "{label}: ebp");
        assert_eq!(native.regs.esi, reference.regs.esi, "{label}: esi");
        assert_eq!(native.regs.edi, reference.regs.edi, "{label}: edi");
        assert_eq!(native.regs.eip, reference.regs.eip, "{label}: eip");
        assert_eq!(native.regs.flags, reference.regs.flags, "{label}: flags");
        let a = ref_memory.as_slice();
        let b = native_memory.as_slice();
        if let Some(at) = a.iter().zip(b).position(|(x, y)| x != y) {
            panic!(
                "{label}: first memory difference at {:#010x}: {:#04x} vs {:#04x}",
                IMAGE_BASE as usize + at,
                a[at],
                b[at]
            );
        }
    }

    #[test]
    fn hercules_keyed_row_matches_the_interpreter_exactly() {
        let width = 37_i32;
        let make = || {
            let (memory, cpu) = row_machine(HERCULES_KEYED_ROW, KEYED_ROW_BYTES);
            let mut cpu = cpu;
            cpu.regs.ebx = (-(width as i64)) as u32;
            cpu.regs.esi = ROW_SRC.wrapping_add(width as u32);
            cpu.regs.edi = ROW_DST.wrapping_add(width as u32);
            cpu.regs.eax = 0x1234_5678;
            cpu.regs.ecx = 0x0bad_f00d;
            cpu.regs.edx = 0xfeed_face;
            cpu.regs.ebp = 0x0102_0304;
            (memory, cpu)
        };
        let (mut ref_memory, mut ref_cpu) = make();
        let steps = interpret_row(&mut ref_memory, &mut ref_cpu, HERCULES_KEYED_EXIT);
        let (mut native_memory, mut native_cpu) = make();
        let work = HerculesBlitter::new()
            .keyed_row(&mut native_memory, &mut native_cpu)
            .unwrap();
        assert_row_equivalent(
            "keyed row",
            &ref_cpu,
            &native_cpu,
            &ref_memory,
            &native_memory,
            steps,
            work,
        );
    }

    #[test]
    fn hercules_remap_row_matches_the_interpreter_exactly() {
        let width = 41_i32;
        let make = || {
            let (memory, cpu) = row_machine(HERCULES_REMAP_ROW, REMAP_ROW_BYTES);
            let mut cpu = cpu;
            cpu.regs.ebx = (-(width as i64)) as u32;
            cpu.regs.esi = ROW_SRC.wrapping_add(width as u32);
            cpu.regs.edi = ROW_DST.wrapping_add(width as u32);
            // The row setup does `xor eax,eax` before the loop.
            cpu.regs.eax = 0;
            cpu.regs.ecx = ROW_LUT;
            cpu.regs.edx = 0xfeed_face;
            cpu.regs.ebp = 0x0102_0304;
            (memory, cpu)
        };
        let (mut ref_memory, mut ref_cpu) = make();
        let steps = interpret_row(&mut ref_memory, &mut ref_cpu, HERCULES_REMAP_EXIT);
        let (mut native_memory, mut native_cpu) = make();
        let work = HerculesBlitter::new()
            .remap_row(&mut native_memory, &mut native_cpu)
            .unwrap();
        assert_row_equivalent(
            "remap row",
            &ref_cpu,
            &native_cpu,
            &ref_memory,
            &native_memory,
            steps,
            work,
        );
    }

    #[test]
    fn hercules_scaled_row_matches_the_interpreter_exactly() {
        let make = || {
            let (mut memory, cpu) = row_machine(HERCULES_SCALED_ROW, SCALED_ROW_BYTES);
            memory.write_u32(HERCULES_Y_STEP, 0xffff_0000).unwrap();
            memory.write_u32(HERCULES_X_STEP, 0x0000_0001).unwrap();
            memory.write_u32(HERCULES_SCALE_LUT, ROW_SHADE).unwrap();
            let mut cpu = cpu;
            // (fixed-point coordinate << 16) | row pixel count
            cpu.regs.eax = 0x0123_0020;
            cpu.regs.ebx = 0x0456_0078;
            cpu.regs.ecx = 0x0000_0040;
            cpu.regs.edx = 0;
            cpu.regs.esi = ROW_SRC;
            cpu.regs.edi = ROW_DST;
            cpu.regs.ebp = ROW_REMAP;
            (memory, cpu)
        };
        let (mut ref_memory, mut ref_cpu) = make();
        let steps = interpret_row(&mut ref_memory, &mut ref_cpu, HERCULES_SCALED_EXIT);
        let (mut native_memory, mut native_cpu) = make();
        let work = HerculesBlitter::new()
            .scaled_row(&mut native_memory, &mut native_cpu)
            .unwrap();
        assert_row_equivalent(
            "scaled row",
            &ref_cpu,
            &native_cpu,
            &ref_memory,
            &native_memory,
            steps,
            work,
        );
    }
}

