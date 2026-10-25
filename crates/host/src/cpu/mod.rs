//! The guest CPU: registers, the decoded instruction model, and the step loop.
//!
//! `decode` turns guest bytes into an [`Instruction`]; `exec` performs it. The
//! split exists so both halves can be tested on their own and so the step loop
//! can report what it could not do instead of desynchronising silently.
//!
//! Scope is the 32-bit integer subset this build actually runs. Floating point,
//! MMX/SSE and 16-bit addressing are out on purpose: the reference image never
//! enables them, and an unsupported byte has to produce a diagnostic naming the
//! address, never a guess.
//!
//! Two conventions the two halves rely on:
//!
//! - Relative targets are resolved by the decoder, which is the only component
//!   that knows both the program counter and the instruction length. A jump
//!   operand is therefore an absolute target address, never a displacement.
//! - Flags are computed eagerly. Lazy flags are a speed lever recorded in the
//!   plan, not a correctness requirement; keeping them exact now is what makes
//!   the decoder and executor testable against one another.

use crate::memory::GuestMemory;
use crate::Result;

pub mod decode;
pub mod exec;

pub use decode::decode;
pub use exec::execute;

pub const EAX: u8 = 0;
pub const ECX: u8 = 1;
pub const EDX: u8 = 2;
pub const EBX: u8 = 3;
pub const ESP: u8 = 4;
pub const EBP: u8 = 5;
pub const ESI: u8 = 6;
pub const EDI: u8 = 7;

/// Segment override selectors, as the prefix bytes encode them.
pub const SEG_ES: u8 = 0x26;
pub const SEG_CS: u8 = 0x2e;
pub const SEG_SS: u8 = 0x36;
pub const SEG_DS: u8 = 0x3e;
pub const SEG_FS: u8 = 0x64;
pub const SEG_GS: u8 = 0x65;

/// EFLAGS bits this core maintains. The rest are reported as constant zero by
/// [`Flags::to_eflags`] except bit 1, which is always set on x86.
pub const FLAG_CF: u32 = 1 << 0;
pub const FLAG_PF: u32 = 1 << 2;
pub const FLAG_AF: u32 = 1 << 4;
pub const FLAG_ZF: u32 = 1 << 6;
pub const FLAG_SF: u32 = 1 << 7;
pub const FLAG_DF: u32 = 1 << 10;
pub const FLAG_OF: u32 = 1 << 11;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Byte,
    /// 16-bit operand size, selected by the `0x66` prefix. The reference build
    /// does execute these (the CRT's locale and string helpers use them), so the
    /// model carries the width rather than rejecting the prefix.
    Word,
    Dword,
}

impl Size {
    pub fn bytes(self) -> u32 {
        match self {
            Size::Byte => 1,
            Size::Word => 2,
            Size::Dword => 4,
        }
    }
}

/// `disp(base, index, scale)` as decoded. `base` and `index` are register
/// indices; a fixed displacement has neither.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mem {
    pub base: Option<u8>,
    pub index: Option<u8>,
    pub scale: u8,
    pub disp: i32,
    /// Only `fs:` is meaningful in this image, where `fs:0` is the TEB.
    pub segment: Option<u8>,
}

impl Mem {
    pub const fn direct(disp: u32) -> Self {
        Self {
            base: None,
            index: None,
            scale: 1,
            disp: disp as i32,
            segment: None,
        }
    }
}

/// One operand. `Imm8` is carried unsigned and sign-extended by the executor
/// when the operation is 32-bit, which is what `83 /n` and `6A` require.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operand {
    None,
    Reg(u8),
    /// `ax`, `cx`, `dx`, `bx`, `sp`, `bp`, `si`, `di` under a `0x66` prefix.
    Reg16(u8),
    RegByte(u8),
    HighByte(u8),
    Mem(Mem),
    Imm8(u8),
    Imm16(u16),
    Imm32(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    Overflow,
    NotOverflow,
    Below,
    AboveOrEqual,
    Equal,
    NotEqual,
    BelowOrEqual,
    Above,
    Sign,
    NotSign,
    Parity,
    NotParity,
    Less,
    GreaterOrEqual,
    LessOrEqual,
    Greater,
}

/// `rep`/`repe` and `repne` prefixes. The executor uses it for the string
/// instructions only; elsewhere it is ignored, as the hardware does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Repeat {
    None,
    WhileZero,
    WhileNotZero,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mnemonic {
    Mov,
    /// Source width is [`Instruction::size`]; the destination is always dword.
    Movzx,
    Movsx,
    Lea,
    Xchg,
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Cmp,
    Test,
    Inc,
    Dec,
    Neg,
    Not,
    Mul,
    Imul,
    Div,
    Idiv,
    Shl,
    Shr,
    Sar,
    Rol,
    Ror,
    Rcl,
    Rcr,
    Shld,
    Shrd,
    Push,
    Pop,
    Pushad,
    Popad,
    /// `push es` and friends. Segments are flat here, so the pushed selector is
    /// zero: the guest saves and restores a value it never uses as a base.
    PushSeg,
    PopSeg,
    /// Flag-only instructions. `cli`/`sti` do nothing here: this core has no
    /// interrupt path, so an enabled-interrupt state cannot change behaviour.
    Clc,
    Stc,
    Cmc,
    Cli,
    Sti,
    Cld,
    Std,
    /// `bt`/`bts`/`btr`/`btc`: CF gets the tested bit, and the three write forms
    /// also set, clear or invert it.
    Bt,
    Bts,
    Btr,
    Btc,
    /// `loop`, `loope`, `loopne` and `jecxz`, all with an 8-bit displacement.
    Loop,
    Loope,
    Loopne,
    Jecxz,
    Jmp,
    Jcc(Condition),
    Call,
    Ret,
    Leave,
    Nop,
    Setcc(Condition),
    Cmovcc(Condition),
    Cwde,
    Cdq,
    /// `xlat`: `al = [ebx + al]`, which MSVC uses to index a byte table.
    Xlat,
    Lahf,
    Sahf,
    /// x87. Memory forms carry their width in [`Instruction::size`]; stack
    /// forms name `st(i)` with [`Operand::Imm8`], and two-operand forms put the
    /// destination in operand 1.
    Fld,
    Fst,
    Fstp,
    Fild,
    Fist,
    Fistp,
    Fadd,
    Fmul,
    Fsub,
    Fsubr,
    Fdiv,
    Fdivr,
    Faddp,
    Fmulp,
    Fsubp,
    Fsubrp,
    Fdivp,
    Fdivrp,
    Fcom,
    Fcomp,
    Fucom,
    Fucomp,
    Fcompp,
    Fldz,
    Fld1,
    Fxch,
    Fchs,
    Fabs,
    Fsqrt,
    /// `fsin` and `fcos`: the sine and cosine of `st(0)`, in radians.
    Fsin,
    Fcos,
    /// `fdecstp` and `fincstp`: rotate the register stack without touching data.
    Fdecstp,
    Fincstp,
    Fnstsw,
    Fnstcw,
    Fldcw,
    Fninit,
    /// `fnclex`: clears the exception flags, leaving the control word alone.
    Fnclex,
    /// `fwait`, which the CRT emits before some x87 instructions. It is a
    /// separate instruction here rather than a prefix, so `9b db e3` (`finit`)
    /// decodes as a wait followed by an initialise.
    Wait,
    Pushfd,
    Popfd,
    Hlt,
    Int3,
    /// String operations take no operands: the address registers and the
    /// direction flag carry the state.
    Movs,
    Stos,
    Lods,
    Cmps,
    Scas,
}

/// A decoded instruction. `len` is what the step loop adds to `eip`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Instruction {
    pub mnemonic: Mnemonic,
    pub size: Size,
    pub operands: [Operand; 3],
    pub repeat: Repeat,
    /// True for the x87 forms whose memory operand is 64 bits wide. Keeping this
    /// out of [`Size`] means no integer path ever has to consider a width it
    /// cannot produce.
    pub double: bool,
    /// Set by the decoder for every floating-point instruction, so the executor
    /// takes one branch instead of matching thirty-three mnemonics per step.
    pub x87: bool,
    /// True for the x87 forms whose memory operand is an integer (`fiadd`,
    /// `fimul`, `ficom` and their siblings), which the board's arithmetic uses
    /// as readily as the float ones.
    pub integer: bool,
    pub len: u8,
    /// Address of the first byte, kept for diagnostics.
    pub pc: u32,
}

impl Instruction {
    pub fn new(
        pc: u32,
        len: u8,
        mnemonic: Mnemonic,
        size: Size,
        operands: [Operand; 3],
    ) -> Self {
        Self {
            mnemonic,
            size,
            operands,
            repeat: Repeat::None,
            double: false,
            x87: false,
            integer: false,
            len,
            pc,
        }
    }

    /// An instruction with no operands, such as `pushfd` or `cdq`.
    pub fn plain(pc: u32, len: u8, mnemonic: Mnemonic, size: Size) -> Self {
        Self::new(pc, len, mnemonic, size, [Operand::None; 3])
    }

    pub fn operand(&self, index: usize) -> Operand {
        self.operands.get(index).copied().unwrap_or(Operand::None)
    }

    /// One mnemonic plus operands, for diagnostics and test expectations.
    pub fn describe(&self) -> String {
        let mut text = format!("{:?}", self.mnemonic);
        let mut first = true;
        for operand in self.operands {
            if operand == Operand::None {
                continue;
            }
            text.push_str(if first { " " } else { ", " });
            text.push_str(&format!("{operand:?}"));
            first = false;
        }
        if self.repeat != Repeat::None {
            text = format!("{:?} {text}", self.repeat);
        }
        text
    }
}

impl Default for Instruction {
    fn default() -> Self {
        Self::plain(0, 1, Mnemonic::Nop, Size::Dword)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Flags {
    pub cf: bool,
    pub pf: bool,
    pub af: bool,
    pub zf: bool,
    pub sf: bool,
    pub of: bool,
    pub df: bool,
}

impl Flags {
    /// Packs the arithmetic flags into an EFLAGS image suitable for `pushfd`.
    pub fn to_eflags(self) -> u32 {
        let mut value = 0x2;
        if self.cf {
            value |= FLAG_CF;
        }
        if self.pf {
            value |= FLAG_PF;
        }
        if self.af {
            value |= FLAG_AF;
        }
        if self.zf {
            value |= FLAG_ZF;
        }
        if self.sf {
            value |= FLAG_SF;
        }
        if self.df {
            value |= FLAG_DF;
        }
        if self.of {
            value |= FLAG_OF;
        }
        value
    }

    /// Reads back only the bits this core maintains; everything else is dropped
    /// rather than stored, so `popfd` cannot put the core into a state it does
    /// not model.
    pub fn from_eflags(value: u32) -> Self {
        Self {
            cf: value & FLAG_CF != 0,
            pf: value & FLAG_PF != 0,
            af: value & FLAG_AF != 0,
            zf: value & FLAG_ZF != 0,
            sf: value & FLAG_SF != 0,
            df: value & FLAG_DF != 0,
            of: value & FLAG_OF != 0,
        }
    }

    /// Parity of the low byte, as `PF` is defined.
    pub fn parity(value: u32) -> bool {
        (value as u8).count_ones() % 2 == 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Registers {
    pub eax: u32,
    pub ecx: u32,
    pub edx: u32,
    pub ebx: u32,
    pub esp: u32,
    pub ebp: u32,
    pub esi: u32,
    pub edi: u32,
    pub eip: u32,
    pub flags: Flags,
}

impl Registers {
    pub fn get(&self, index: u8) -> u32 {
        match index & 7 {
            EAX => self.eax,
            ECX => self.ecx,
            EDX => self.edx,
            EBX => self.ebx,
            ESP => self.esp,
            EBP => self.ebp,
            ESI => self.esi,
            _ => self.edi,
        }
    }

    pub fn set(&mut self, index: u8, value: u32) {
        match index & 7 {
            EAX => self.eax = value,
            ECX => self.ecx = value,
            EDX => self.edx = value,
            EBX => self.ebx = value,
            ESP => self.esp = value,
            EBP => self.ebp = value,
            ESI => self.esi = value,
            _ => self.edi = value,
        }
    }

    /// Low word of `ax`, `cx`, `dx`, `bx`.
    pub fn get_word(&self, index: u8) -> u16 {
        self.get(index) as u16
    }

    /// Writes the low word and zeroes the upper half, which is what a 16-bit
    /// write does in 32-bit mode. The guest relies on it: `imul ax, cx` followed
    /// by `mov ecx, eax` reads the zeroed upper half as part of the value.
    pub fn set_word(&mut self, index: u8, value: u16) {
        self.set(index, u32::from(value));
    }

    /// Low byte of `al`, `cl`, `dl`, `bl`.
    pub fn get_byte(&self, index: u8) -> u8 {
        self.get(index) as u8
    }

    pub fn set_byte(&mut self, index: u8, value: u8) {
        let register = self.get(index) & !0xff;
        self.set(index, register | u32::from(value));
    }

    /// `ah`, `ch`, `dh`, `bh`.
    pub fn get_high_byte(&self, index: u8) -> u8 {
        (self.get(index) >> 8) as u8
    }

    pub fn set_high_byte(&mut self, index: u8, value: u8) {
        let register = self.get(index) & !0xff00;
        self.set(index, register | (u32::from(value) << 8));
    }

    pub fn eflags(&self) -> u32 {
        self.flags.to_eflags()
    }
}

/// Where the host puts its import stubs in the guest's address space.
///
/// Every IAT slot is filled with `HOST_FUNCTION_BASE + index * 4` when the image
/// is mapped, so a pointer loaded out of the import table and called through a
/// register lands somewhere the step loop recognises. Nothing is mapped below
/// [`crate::pe::IMAGE_BASE`], which is why this range is unambiguous.
pub const HOST_FUNCTION_BASE: u32 = 0x00f0_0000;

/// Where a host-called guest routine returns to. A timer callback is entered by
/// the session with this address pushed as its `ret` target, so the interpreter
/// can tell a callback's return from the guest's own and restore the frame it
/// interrupted. It is far above the import stubs, so no slot lands on it.
pub const TIMER_TRAMPOLINE: u32 = HOST_FUNCTION_BASE + 0x0100_0000;

/// Where a guest routine called *by a shim* returns to: the same idea as the
/// timer trampoline, for the window procedures and dialog procedures an import
/// asks the guest to run. Its own address so the session can tell the two apart
/// when both are live (a timer can fire inside a dispatched message).
pub const CALL_TRAMPOLINE: u32 = HOST_FUNCTION_BASE + 0x0100_0004;

/// The import slot a program counter points at, when it lands on a host stub.
pub fn host_slot_at(eip: u32, table: crate::dispatch::Table) -> Option<usize> {
    // One subtraction and one compare on the hot path; the modulo and divide
    // are only reached inside the stub range.
    let offset = eip.wrapping_sub(HOST_FUNCTION_BASE);
    if offset >= table.len() as u32 * 4 {
        return None;
    }
    // A stub is four bytes wide; a program counter inside one but not at its
    // start is not a call site, and treating it as one would run the wrong
    // import.
    if offset & 3 != 0 {
        return None;
    }
    let slot = (offset / 4) as usize;
    (slot < table.len()).then_some(slot)
}

pub const FPU_STACK: usize = 8;

/// The x87 register stack, kept as `f64`.
///
/// Double precision is enough for what the reference image computes (a version
/// comparison and a handful of derived constants); the 80-bit temporary format
/// is not modelled, so a computation that depends on extended precision would
/// round differently here. Nothing in the boot does.
#[derive(Clone, Debug)]
pub struct Fpu {
    stack: [f64; FPU_STACK],
    top: u8,
    pub control: u16,
    /// Status word, of which only C0/C2/C3 (bits 8-10) are maintained.
    pub status: u16,
}

impl Default for Fpu {
    fn default() -> Self {
        Self {
            stack: [0.0; FPU_STACK],
            top: 0,
            control: 0x037f,
            status: 0,
        }
    }
}

impl Fpu {
    pub fn top(&self) -> u8 {
        self.top
    }

    pub fn st(&self, index: u8) -> f64 {
        self.stack[(usize::from(self.top) + usize::from(index)) % FPU_STACK]
    }

    pub fn set_st(&mut self, index: u8, value: f64) {
        self.stack[(usize::from(self.top) + usize::from(index)) % FPU_STACK] = value;
    }

    pub fn push(&mut self, value: f64) {
        self.top = (self.top + FPU_STACK as u8 - 1) % FPU_STACK as u8;
        self.stack[usize::from(self.top)] = value;
    }

    pub fn pop(&mut self) {
        self.top = (self.top + 1) % FPU_STACK as u8;
    }

    /// Rotates the register stack without touching the data, which is what
    /// `fdecstp` (-1) and `fincstp` (+1) do.
    pub fn rotate(&mut self, delta: i8) {
        let size = FPU_STACK as i8;
        self.top = (((i8::try_from(self.top).unwrap_or(0) + delta) % size + size) % size) as u8;
    }

    /// `fnstsw` images: C0 in bit 8, C2 in bit 9, C3 in bit 10, as the hardware
    /// reports them so `sahf` can turn them into CF/PF/ZF.
    pub fn compare_status(&mut self, ordering: Option<std::cmp::Ordering>) {
        self.status &= !0x4700;
        match ordering {
            None => self.status |= 0x4500,
            Some(std::cmp::Ordering::Greater) => {}
            Some(std::cmp::Ordering::Less) => self.status |= 0x0100,
            Some(std::cmp::Ordering::Equal) => self.status |= 0x4000,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    pub fn stack(&self) -> [f64; FPU_STACK] {
        self.stack
    }
}

/// Entries in the decoded-instruction cache. It is direct-mapped and indexed by
/// address, so a collision costs a decode and never a wrong answer.
const DECODE_CACHE_ENTRIES: usize = 1 << 14;

/// Machine state that is not a register. `instructions` is what the diagnostics
/// page and the throughput measurement read.
#[derive(Clone, Debug, Default)]
pub struct Cpu {
    pub regs: Registers,
    pub fpu: Fpu,
    /// The target's import surface, which decides whether an address is a host
    /// stub. Copied in because a table is a `&'static` slice.
    pub table: crate::dispatch::Table,
    pub instructions: u64,
    /// Instructions already decoded, by address.
    ///
    /// The guest runs the same loops millions of times -- its drawing and its
    /// blitting loops are most of what this interpreter executes -- and decoding
    /// an instruction costs about as much as running it. `.text` is read-only in
    /// this host (a write there is refused), so a decoded instruction cannot go
    /// stale; the only writes to code happen before the first instruction runs,
    /// when the loader maps the image and the import table.
    decode_cache: Vec<Option<(u32, Instruction)>>,
}

impl Cpu {
    /// A CPU with no import surface, for tests that do not call out.
    pub fn at(entry: u32) -> Self {
        Self::with_table(entry, crate::dispatch::Table::EMPTY)
    }

    pub fn with_table(entry: u32, table: crate::dispatch::Table) -> Self {
        Self {
            regs: Registers {
                eip: entry,
                ..Registers::default()
            },
            fpu: Fpu::default(),
            table,
            instructions: 0,
            decode_cache: vec![None; DECODE_CACHE_ENTRIES],
        }
    }

    /// The instruction at `pc`, from the cache when it is there.
    #[inline]
    fn fetch(&mut self, memory: &GuestMemory, pc: u32) -> Result<Instruction> {
        let slot = (pc as usize >> 1) & (DECODE_CACHE_ENTRIES - 1);
        if let Some((cached_pc, instruction)) = self.decode_cache[slot] {
            if cached_pc == pc {
                return Ok(instruction);
            }
        }
        let instruction = decode(memory, pc)?;
        self.decode_cache[slot] = Some((pc, instruction));
        Ok(instruction)
    }

    pub fn registers(&self) -> [(u8, u32); 8] {
        [
            (EAX, self.regs.eax),
            (ECX, self.regs.ecx),
            (EDX, self.regs.edx),
            (EBX, self.regs.ebx),
            (ESP, self.regs.esp),
            (EBP, self.regs.ebp),
            (ESI, self.regs.esi),
            (EDI, self.regs.edi),
        ]
    }
}

/// A call through an IAT slot: the guest has asked for a host function.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HostCall {
    /// Index into the dispatch table / the image's import list, or 0 for a call
    /// through a host object. [`HostCall::thunk`] says which of the two it is.
    pub slot: usize,
    /// The interface method this call went through, when the guest called a host
    /// object instead of an import slot.
    pub thunk: Option<(u8, u8)>,
    pub iat_va: u32,
    /// The value the `call` pushed; where execution resumes after the host
    /// function returns.
    pub return_address: u32,
    /// Guest address of the first argument: the caller's ESP with the arguments
    /// already pushed, before the call pushed its return address.
    pub args: u32,
    /// ESP after the import returns and pops its arguments (stdcall).
    pub esp: u32,
    /// Argument bytes the callee pops, when the call sites agreed. `None` means
    /// the shim has to decide the cleanup itself; nothing in the table may be
    /// guessed.
    pub arg_bytes: Option<u32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    Continue,
    Halt,
    HostCall(HostCall),
}

/// Decodes and executes one instruction.
///
/// `eip` is advanced before the executor runs, so a fault leaves the CPU at the
/// instruction that faulted and the address in the error names it.
pub fn step(memory: &mut GuestMemory, cpu: &mut Cpu) -> Result<Step> {
    let pc = cpu.regs.eip;
    // Host stubs and object thunks both live above the last guest section, so a
    // normal instruction pays one compare instead of two range scans.
    if pc >= crate::thunks::THUNK_BASE {
        if let Some(slot) = host_slot_at(pc, cpu.table) {
            // The guest reached a host stub indirectly (`call ebp` after loading
            // the slot), so the return address is already on the stack and the
            // arguments sit above it.
            let entry = &cpu.table.slots[slot];
            let resume = memory.read_u32(cpu.regs.esp)?;
            let args = cpu.regs.esp.wrapping_add(4);
            let arg_bytes = entry.arg_bytes();
            cpu.regs.eip = resume;
            cpu.regs.esp = args.wrapping_add(arg_bytes.unwrap_or(0));
            return Ok(Step::HostCall(HostCall {
                slot,
                thunk: None,
                iat_va: entry.iat_va,
                return_address: resume,
                args,
                esp: cpu.regs.esp,
                arg_bytes,
            }));
        }
        if let Some((kind, method)) = crate::thunks::at(pc) {
            // A call through a vtable the host filled in: the return address is on
            // the stack and `this` is argument 0, exactly as for an import.
            let resume = memory.read_u32(cpu.regs.esp)?;
            let args = cpu.regs.esp.wrapping_add(4);
            let arg_bytes = crate::thunks::arg_bytes(kind, method);
            cpu.regs.eip = resume;
            cpu.regs.esp = args.wrapping_add(arg_bytes);
            return Ok(Step::HostCall(HostCall {
                slot: 0,
                thunk: Some((kind, method)),
                iat_va: pc,
                return_address: resume,
                args,
                esp: cpu.regs.esp,
                arg_bytes: Some(arg_bytes),
            }));
        }
    }
    let instruction = cpu.fetch(memory, pc)?;
    cpu.regs.eip = pc.wrapping_add(u32::from(instruction.len));
    cpu.instructions += 1;
    execute(memory, cpu, &instruction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pe::IMAGE_BASE;

    #[test]
    fn register_halves_read_and_write_independently() {
        let mut registers = Registers::default();
        registers.set(EAX, 0x1122_3344);
        assert_eq!(registers.get_word(EAX), 0x3344);
        assert_eq!(registers.get_byte(EAX), 0x44);
        assert_eq!(registers.get_high_byte(EAX), 0x33);
        registers.set_word(EAX, 0xbbaa);
        assert_eq!(registers.get(EAX), 0x0000_bbaa, "a word write zeroes the upper half");
        registers.set(EAX, 0x1122_3344);
        registers.set_byte(EAX, 0xaa);
        registers.set_high_byte(EAX, 0xbb);
        assert_eq!(registers.get(EAX), 0x1122_bbaa);
    }

    #[test]
    fn eflags_round_trip_keeps_only_modelled_flags() {
        let flags = Flags {
            cf: true,
            zf: true,
            sf: true,
            of: true,
            ..Flags::default()
        };
        let image = flags.to_eflags();
        assert_eq!(image & 0x2, 0x2, "bit 1 is always set");
        assert_eq!(Flags::from_eflags(image), flags);

        // Bits the core does not model must not survive a popfd/pushfd cycle.
        let stripped = Flags::from_eflags(0xffff_ffff ^ (FLAG_CF | FLAG_ZF));
        assert!(!stripped.cf && !stripped.zf);
        assert!(stripped.df);
    }

    #[test]
    fn parity_covers_the_low_byte() {
        assert!(Flags::parity(0x00));
        assert!(!Flags::parity(0x01));
        assert!(Flags::parity(0xffff_ff03));
    }

    #[test]
    fn describing_an_instruction_lists_its_operands() {
        let instruction = Instruction::new(
            0x0040_1000,
            3,
            Mnemonic::Mov,
            Size::Dword,
            [Operand::Reg(EAX), Operand::Mem(Mem::direct(0x0040_2000)), Operand::None],
        );
        assert_eq!(
            instruction.describe(),
            "Mov Reg(0), Mem(Mem { base: None, index: None, scale: 1, disp: 4202496, segment: None })"
        );
    }

    /// A one-slot table, shaped like a generated one, so the stub arithmetic is
    /// tested without a real executable.
    static SLOTS: [crate::dispatch::Slot; 1] = [crate::dispatch::Slot {
        dll: "KERNEL32.dll",
        name: "ReadFile",
        iat_va: 0x0040_1000,
        args: Some(20),
        sites: 2,
        agreement: 2,
        ambiguous: 0,
    }];

    static TABLE: crate::dispatch::Table = crate::dispatch::Table { slots: &SLOTS };

    #[test]
    fn a_pc_on_a_host_stub_is_an_indirect_host_call() {
        let mut memory = GuestMemory::empty(0x1000).expect("blank memory");
        let slot = 0;
        let stub = HOST_FUNCTION_BASE + slot as u32 * 4;
        assert_eq!(host_slot_at(stub, TABLE), Some(slot));
        assert_eq!(host_slot_at(stub + 2, TABLE), None);
        assert_eq!(host_slot_at(HOST_FUNCTION_BASE + TABLE.len() as u32 * 4, TABLE), None);

        // The guest pushed its return address and is now at the stub.
        let stack = IMAGE_BASE + 0x800;
        memory.write_u32(stack, 0x0040_1234).unwrap();
        let mut cpu = Cpu::with_table(stub, TABLE);
        cpu.regs.esp = stack;

        match step(&mut memory, &mut cpu).expect("host call") {
            Step::HostCall(call) => {
                assert_eq!(call.slot, slot);
                assert_eq!(call.return_address, 0x0040_1234);
                assert_eq!(call.args, stack + 4);
                assert_eq!(call.arg_bytes, Some(20));
            }
            other => panic!("expected a host call, got {other:?}"),
        }
        assert_eq!(cpu.regs.eip, 0x0040_1234, "the host returns to the caller");
        assert_eq!(cpu.regs.esp, stack + 4 + 20, "stdcall pops the arguments");
    }

    #[test]
    fn steps_through_memory_and_reports_the_program_counter() {
        let mut memory = GuestMemory::empty(0x1000).expect("blank memory");
        // mov eax, 7; inc eax; ret
        memory
            .write_bytes(IMAGE_BASE, &[0xb8, 7, 0, 0, 0, 0x40, 0xc3])
            .unwrap();
        let mut cpu = Cpu::at(IMAGE_BASE);

        assert_eq!(step(&mut memory, &mut cpu), Ok(Step::Continue));
        assert_eq!(cpu.regs.eax, 7);
        assert_eq!(cpu.regs.eip, IMAGE_BASE + 5);
        assert_eq!(step(&mut memory, &mut cpu), Ok(Step::Continue));
        assert_eq!(cpu.regs.eax, 8);
        assert_eq!(cpu.instructions, 2);
    }
}
