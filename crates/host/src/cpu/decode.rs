//! Decoder for the guest's 32-bit integer instruction set.
//!
//! The decoder is the only component that knows both the program counter and an
//! instruction's length, so it resolves every relative branch into an absolute
//! target. The executor therefore never adds a displacement: a jump operand is
//! an address.
//!
//! Coverage is the subset this build runs; anything else returns an error that
//! names the bytes and the address rather than decoding a neighbouring
//! instruction. The `0x66` prefix selects the 16-bit operand size, which the
//! reference build really executes, so the width is carried through every
//! operand that depends on it. `0x67` (16-bit addressing) is still rejected:
//! the model has no 16-bit addressing modes. A few `0x66` encodings whose
//! operation changes shape rather than width (`cbw`, `cwd`, `pushad`, 16-bit
//! near calls and jumps) are rejected with their address instead of being run
//! at the wrong width.

use crate::cpu::{
    Condition, Instruction, Mem, Mnemonic, Operand, Repeat, Size, EAX, ECX,
};
use crate::memory::GuestMemory;
use crate::Result;

/// Longest x86 instruction, prefixes included.
const MAX_INSTRUCTION: u32 = 15;
/// More than this many prefixes is a decode error, as on the hardware.
const MAX_PREFIXES: u32 = 4;

const ALU: [Mnemonic; 8] = [
    Mnemonic::Add,
    Mnemonic::Or,
    Mnemonic::Adc,
    Mnemonic::Sbb,
    Mnemonic::And,
    Mnemonic::Sub,
    Mnemonic::Xor,
    Mnemonic::Cmp,
];

const SHIFTS: [Mnemonic; 8] = [
    Mnemonic::Rol,
    Mnemonic::Ror,
    Mnemonic::Rcl,
    Mnemonic::Rcr,
    Mnemonic::Shl,
    Mnemonic::Shr,
    Mnemonic::Shl,
    Mnemonic::Sar,
];

const GROUP3: [Mnemonic; 8] = [
    Mnemonic::Test,
    Mnemonic::Test,
    Mnemonic::Not,
    Mnemonic::Neg,
    Mnemonic::Mul,
    Mnemonic::Imul,
    Mnemonic::Div,
    Mnemonic::Idiv,
];

const CONDITIONS: [Condition; 16] = [
    Condition::Overflow,
    Condition::NotOverflow,
    Condition::Below,
    Condition::AboveOrEqual,
    Condition::Equal,
    Condition::NotEqual,
    Condition::BelowOrEqual,
    Condition::Above,
    Condition::Sign,
    Condition::NotSign,
    Condition::Parity,
    Condition::NotParity,
    Condition::Less,
    Condition::GreaterOrEqual,
    Condition::LessOrEqual,
    Condition::Greater,
];

fn unsupported(pc: u32, what: &str) -> String {
    format!("unsupported {what} at {pc:#010x}")
}

/// A read cursor that enforces the 15-byte limit and reports the address of the
/// byte it could not read.
struct Cursor<'a> {
    memory: &'a GuestMemory,
    pc: u32,
    offset: u32,
}

impl<'a> Cursor<'a> {
    fn new(memory: &'a GuestMemory, pc: u32) -> Self {
        Self {
            memory,
            pc,
            offset: 0,
        }
    }

    fn at(&self) -> u32 {
        self.pc.wrapping_add(self.offset)
    }

    fn peek(&self) -> Result<u8> {
        self.memory.read_u8(self.at())
    }

    fn next(&mut self) -> Result<u8> {
        if self.offset >= MAX_INSTRUCTION {
            return Err(format!(
                "instruction at {:#010x} is longer than {MAX_INSTRUCTION} bytes",
                self.pc
            ));
        }
        let byte = self.memory.read_u8(self.at())?;
        self.offset += 1;
        Ok(byte)
    }

    fn u16(&mut self) -> Result<u16> {
        let low = u16::from(self.next()?);
        let high = u16::from(self.next()?);
        Ok(low | (high << 8))
    }

    fn u32(&mut self) -> Result<u32> {
        let mut value = 0_u32;
        for shift in (0..32).step_by(8) {
            value |= u32::from(self.next()?) << shift;
        }
        Ok(value)
    }

    fn i8(&mut self) -> Result<i8> {
        Ok(self.next()? as i8)
    }

    fn i32(&mut self) -> Result<i32> {
        Ok(self.u32()? as i32)
    }

    fn len(&self) -> u8 {
        self.offset as u8
    }
}

struct ModRm {
    operand: Operand,
    reg: u8,
    register: bool,
}

/// `ah`, `ch`, `dh`, `bh` for indices 4-7, the low byte registers otherwise.
fn byte_operand(register: u8) -> Operand {
    if register < 4 {
        Operand::RegByte(register)
    } else {
        Operand::HighByte(register - 4)
    }
}

fn register_operand(register: u8, size: Size) -> Operand {
    match size {
        Size::Dword => Operand::Reg(register),
        Size::Word => Operand::Reg16(register),
        Size::Byte => byte_operand(register),
    }
}

/// Decodes the ModRM byte and whatever addressing bytes follow it.
fn modrm(cursor: &mut Cursor, size: Size, segment: Option<u8>) -> Result<ModRm> {
    let byte = cursor.next()?;
    let mode = byte >> 6;
    let reg = (byte >> 3) & 7;
    let rm = byte & 7;

    if mode == 3 {
        return Ok(ModRm {
            operand: register_operand(rm, size),
            reg,
            register: true,
        });
    }

    let mut base = None;
    let mut index = None;
    let mut scale = 1;
    let mut disp = 0_i32;

    if rm == 4 {
        let sib = cursor.next()?;
        scale = 1 << (sib >> 6);
        let index_register = (sib >> 3) & 7;
        if index_register != 4 {
            index = Some(index_register);
        }
        let base_register = sib & 7;
        if base_register == 5 && mode == 0 {
            // No base, just a displacement: this is how a plain absolute
            // address with a scaled index is encoded.
            disp = cursor.i32()?;
        } else {
            base = Some(base_register);
        }
    } else if rm == 5 && mode == 0 {
        disp = cursor.i32()?;
    } else {
        base = Some(rm);
    }

    match mode {
        1 => disp = i32::from(cursor.i8()?),
        2 => disp = cursor.i32()?,
        _ => {}
    }

    Ok(ModRm {
        operand: Operand::Mem(Mem {
            base,
            index,
            scale,
            disp,
            segment,
        }),
        reg,
        register: false,
    })
}

fn group1(cursor: &mut Cursor, pc: u32, size: Size, byte_immediate: bool, segment: Option<u8>) -> Result<Instruction> {
    let decoded = modrm(cursor, size, segment)?;
    let mnemonic = ALU[decoded.reg as usize];
    let immediate = if byte_immediate {
        Operand::Imm8(cursor.next()?)
    } else {
        match size {
            Size::Word => Operand::Imm16(cursor.u16()?),
            _ => Operand::Imm32(cursor.u32()?),
        }
    };
    Ok(Instruction::new(
        pc,
        cursor.len(),
        mnemonic,
        size,
        [decoded.operand, immediate, Operand::None],
    ))
}

enum Count {
    Immediate,
    One,
    Cl,
}

fn group2(cursor: &mut Cursor, pc: u32, size: Size, count: Count, segment: Option<u8>) -> Result<Instruction> {
    let decoded = modrm(cursor, size, segment)?;
    let mnemonic = SHIFTS[decoded.reg as usize];
    let count = match count {
        Count::Immediate => Operand::Imm8(cursor.next()?),
        Count::One => Operand::Imm8(1),
        Count::Cl => Operand::RegByte(ECX),
    };
    Ok(Instruction::new(
        pc,
        cursor.len(),
        mnemonic,
        size,
        [decoded.operand, count, Operand::None],
    ))
}

fn group3(cursor: &mut Cursor, pc: u32, size: Size, segment: Option<u8>) -> Result<Instruction> {
    let decoded = modrm(cursor, size, segment)?;
    let mnemonic = GROUP3[decoded.reg as usize];
    let mut operands = [decoded.operand, Operand::None, Operand::None];
    if mnemonic == Mnemonic::Test {
        operands[1] = match size {
            Size::Byte => Operand::Imm8(cursor.next()?),
            Size::Word => Operand::Imm16(cursor.u16()?),
            Size::Dword => Operand::Imm32(cursor.u32()?),
        };
    }
    Ok(Instruction::new(pc, cursor.len(), mnemonic, size, operands))
}

/// `E8`/`E9`-style displacement, resolved to the address it reaches.
fn relative(cursor: &mut Cursor, displacement: i32) -> Operand {
    Operand::Imm32(cursor.at().wrapping_add(displacement as u32))
}

/// The three bit-test forms that take a register bit index.
fn bit_test_mnemonic(opcode: u8) -> Option<Mnemonic> {
    match opcode {
        0xa3 => Some(Mnemonic::Bt),
        0xab => Some(Mnemonic::Bts),
        0xb3 => Some(Mnemonic::Btr),
        0xbb => Some(Mnemonic::Btc),
        _ => None,
    }
}

fn two_byte(
    cursor: &mut Cursor,
    pc: u32,
    segment: Option<u8>,
    width: Size,
) -> Result<Instruction> {
    let opcode = cursor.next()?;
    match opcode {
        // Multi-byte NOP, which the compiler emits for alignment.
        0x1f => {
            modrm(cursor, Size::Dword, segment)?;
            Ok(Instruction::plain(pc, cursor.len(), Mnemonic::Nop, Size::Dword))
        }
        0x40..=0x4f => {
            if width == Size::Byte {
                return Err(unsupported(pc, "byte cmov"));
            }
            let decoded = modrm(cursor, width, segment)?;
            Ok(Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Cmovcc(CONDITIONS[usize::from(opcode & 0x0f)]),
                width,
                [
                    register_operand(decoded.reg, width),
                    decoded.operand,
                    Operand::None,
                ],
            ))
        }
        0x80..=0x8f => {
            let displacement = cursor.i32()?;
            Ok(Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Jcc(CONDITIONS[usize::from(opcode & 0x0f)]),
                Size::Dword,
                [relative(cursor, displacement), Operand::None, Operand::None],
            ))
        }
        0x90..=0x9f => {
            let decoded = modrm(cursor, Size::Byte, segment)?;
            Ok(Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Setcc(CONDITIONS[usize::from(opcode & 0x0f)]),
                Size::Byte,
                [decoded.operand, Operand::None, Operand::None],
            ))
        }
        0xa3 | 0xab | 0xb3 | 0xbb => {
            let decoded = modrm(cursor, width, segment)?;
            Ok(Instruction::new(
                pc,
                cursor.len(),
                bit_test_mnemonic(opcode).expect("bit test opcode"),
                width,
                [
                    decoded.operand,
                    register_operand(decoded.reg, width),
                    Operand::None,
                ],
            ))
        }
        0xba => {
            let decoded = modrm(cursor, width, segment)?;
            let mnemonic = match decoded.reg {
                4 => Mnemonic::Bt,
                5 => Mnemonic::Bts,
                6 => Mnemonic::Btr,
                7 => Mnemonic::Btc,
                _ => return Err(unsupported(pc, "group 8 with a reserved reg field")),
            };
            let bit = cursor.next()?;
            Ok(Instruction::new(
                pc,
                cursor.len(),
                mnemonic,
                width,
                [decoded.operand, Operand::Imm8(bit), Operand::None],
            ))
        }
        0xa4 | 0xa5 => {
            if width == Size::Word {
                return Err(unsupported(pc, "16-bit shld"));
            }
            let decoded = modrm(cursor, Size::Dword, segment)?;
            let count = if opcode == 0xa4 {
                Operand::Imm8(cursor.next()?)
            } else {
                Operand::RegByte(ECX)
            };
            Ok(Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Shld,
                Size::Dword,
                [decoded.operand, Operand::Reg(decoded.reg), count],
            ))
        }
        0xac | 0xad => {
            if width == Size::Word {
                return Err(unsupported(pc, "16-bit shrd"));
            }
            let decoded = modrm(cursor, Size::Dword, segment)?;
            let count = if opcode == 0xac {
                Operand::Imm8(cursor.next()?)
            } else {
                Operand::RegByte(ECX)
            };
            Ok(Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Shrd,
                Size::Dword,
                [decoded.operand, Operand::Reg(decoded.reg), count],
            ))
        }
        0xaf => {
            if width == Size::Byte {
                return Err(unsupported(pc, "byte imul"));
            }
            let decoded = modrm(cursor, width, segment)?;
            Ok(Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Imul,
                width,
                [
                    register_operand(decoded.reg, width),
                    decoded.operand,
                    Operand::None,
                ],
            ))
        }
        0xb6 | 0xb7 | 0xbe | 0xbf => {
            let mnemonic = if opcode == 0xb6 || opcode == 0xb7 {
                Mnemonic::Movzx
            } else {
                Mnemonic::Movsx
            };
            let source = if opcode == 0xb6 || opcode == 0xbe {
                Size::Byte
            } else {
                Size::Word
            };
            let decoded = modrm(cursor, source, segment)?;
            // Under a `0x66` prefix the destination would be 16-bit. The low
            // half is identical, so this is the one place the width prefix is
            // accepted rather than rejected; only the upper half of the
            // destination differs, and nothing in this build reads it.
            Ok(Instruction::new(
                pc,
                cursor.len(),
                mnemonic,
                source,
                [Operand::Reg(decoded.reg), decoded.operand, Operand::None],
            ))
        }
        _ => Err(unsupported(pc, &format!("opcode 0x0f 0x{opcode:02x}"))),
    }
}

/// Decodes the instruction at `pc`.
pub fn decode(memory: &GuestMemory, pc: u32) -> Result<Instruction> {
    let mut cursor = Cursor::new(memory, pc);
    let mut segment = None;
    let mut repeat = Repeat::None;
    let mut width = Size::Dword;
    let mut prefixes = 0_u32;

    loop {
        match cursor.peek()? {
            0x26 | 0x2e | 0x36 | 0x3e | 0x64 | 0x65 => segment = Some(cursor.next()?),
            0x66 => {
                cursor.next()?;
                width = Size::Word;
            }
            0x67 => return Err(unsupported(pc, "16-bit addressing (0x67)")),
            0xf0 => {
                cursor.next()?;
            }
            0xf2 => {
                cursor.next()?;
                repeat = Repeat::WhileNotZero;
            }
            0xf3 => {
                cursor.next()?;
                repeat = Repeat::WhileZero;
            }
            _ => break,
        }
        prefixes += 1;
        if prefixes > MAX_PREFIXES {
            return Err(format!("more than {MAX_PREFIXES} prefixes at {pc:#010x}"));
        }
    }

    let mut instruction = decode_opcode(&mut cursor, pc, segment, width)?;
    instruction.repeat = repeat;
    Ok(instruction)
}

/// Floating-point escapes `D8`-`DF`.
///
/// Only the forms this build executes are decoded; each one either carries a
/// memory width or names `st(i)`, and the stack forms use operand 1 for the
/// destination so the executor can tell `fadd st, st(i)` from `fadd st(i), st`.
fn x87(cursor: &mut Cursor, pc: u32, opcode: u8) -> Result<Instruction> {
    let decoded = modrm(cursor, Size::Dword, None)?;
    let field = decoded.reg;
    let index = match decoded.operand {
        Operand::Reg(register) => register,
        _ => 0,
    };
    let memory_operand = decoded.operand;
    let register_form = decoded.register;

    let float = |mnemonic: Mnemonic, size: Size, double: bool| {
        let mut instruction = Instruction::new(
            pc,
            cursor.len(),
            mnemonic,
            size,
            [memory_operand, Operand::None, Operand::None],
        );
        instruction.double = double;
        instruction.x87 = true;
        instruction
    };
    // The integer forms reuse the float arithmetic and comparison, with the
    // memory operand read as a signed integer of the instruction's width.
    let float_int = |mnemonic: Mnemonic, size: Size| {
        let mut instruction = float(mnemonic, size, false);
        instruction.integer = true;
        instruction
    };
    let stack = |mnemonic: Mnemonic, operands: [Operand; 3]| {
        let mut instruction = Instruction::new(pc, cursor.len(), mnemonic, Size::Dword, operands);
        instruction.x87 = true;
        instruction
    };

    // Memory forms first: the reg field selects the operation and the width
    // follows the opcode.
    if !register_form {
        return match (opcode, field) {
            (0xd9, 0) => Ok(float(Mnemonic::Fld, Size::Dword, false)),
            (0xd9, 2) => Ok(float(Mnemonic::Fst, Size::Dword, false)),
            (0xd9, 3) => Ok(float(Mnemonic::Fstp, Size::Dword, false)),
            (0xd9, 5) => Ok(float(Mnemonic::Fldcw, Size::Word, false)),
            (0xd9, 7) => Ok(float(Mnemonic::Fnstcw, Size::Word, false)),
            (0xdd, 0) => Ok(float(Mnemonic::Fld, Size::Dword, true)),
            (0xdd, 2) => Ok(float(Mnemonic::Fst, Size::Dword, true)),
            (0xdd, 3) => Ok(float(Mnemonic::Fstp, Size::Dword, true)),
            (0xdb, 0) => Ok(float(Mnemonic::Fild, Size::Dword, false)),
            (0xdb, 2) => Ok(float(Mnemonic::Fist, Size::Dword, false)),
            (0xdb, 3) => Ok(float(Mnemonic::Fistp, Size::Dword, false)),
            (0xdf, 0) => Ok(float(Mnemonic::Fild, Size::Word, false)),
            (0xdf, 2) => Ok(float(Mnemonic::Fist, Size::Word, false)),
            (0xdf, 3) => Ok(float(Mnemonic::Fistp, Size::Word, false)),
            // `da /n`: a 32-bit integer in memory.
            (0xda, 0) => Ok(float(Mnemonic::Fild, Size::Dword, false)),
            (0xda, 1) => Ok(float_int(Mnemonic::Fmul, Size::Dword)),
            (0xda, 2) => Ok(float_int(Mnemonic::Fcom, Size::Dword)),
            (0xda, 3) => Ok(float_int(Mnemonic::Fcomp, Size::Dword)),
            // `de /n`: a 16-bit integer in memory.
            (0xde, 0) => Ok(float_int(Mnemonic::Fadd, Size::Word)),
            (0xde, 1) => Ok(float_int(Mnemonic::Fmul, Size::Word)),
            (0xde, 2) => Ok(float_int(Mnemonic::Fcom, Size::Word)),
            (0xde, 3) => Ok(float_int(Mnemonic::Fcomp, Size::Word)),
            (0xde, 4) => Ok(float_int(Mnemonic::Fsub, Size::Word)),
            (0xde, 5) => Ok(float_int(Mnemonic::Fsubr, Size::Word)),
            (0xde, 6) => Ok(float_int(Mnemonic::Fdiv, Size::Word)),
            (0xde, 7) => Ok(float_int(Mnemonic::Fdivr, Size::Word)),
            (0xdf, 5) => Ok(float(Mnemonic::Fild, Size::Dword, true)),
            (0xdf, 7) => Ok(float(Mnemonic::Fistp, Size::Dword, true)),
            (0xd8 | 0xdc, _) => {
                let double = opcode == 0xdc;
                let mnemonic = match field {
                    0 => Mnemonic::Fadd,
                    1 => Mnemonic::Fmul,
                    2 => Mnemonic::Fcom,
                    3 => Mnemonic::Fcomp,
                    4 => Mnemonic::Fsub,
                    5 => Mnemonic::Fsubr,
                    6 => Mnemonic::Fdiv,
                    _ => Mnemonic::Fdivr,
                };
                Ok(float(mnemonic, Size::Dword, double))
            }
            _ => Err(unsupported(
                pc,
                &format!("x87 memory form 0x{opcode:02x} /{field}"),
            )),
        };
    }

    // Register forms.
    match (opcode, field) {
        (0xd8, 0..=7) => {
            let mnemonic = match field {
                0 => Mnemonic::Fadd,
                1 => Mnemonic::Fmul,
                2 => Mnemonic::Fcom,
                3 => Mnemonic::Fcomp,
                4 => Mnemonic::Fsub,
                5 => Mnemonic::Fsubr,
                6 => Mnemonic::Fdiv,
                _ => Mnemonic::Fdivr,
            };
            Ok(stack(mnemonic, [Operand::Imm8(index), Operand::None, Operand::None]))
        }
        (0xdc, 0..=7) => {
            let mnemonic = match field {
                0 => Mnemonic::Fadd,
                1 => Mnemonic::Fmul,
                2 => Mnemonic::Fcom,
                3 => Mnemonic::Fcomp,
                4 => Mnemonic::Fsub,
                5 => Mnemonic::Fsubr,
                6 => Mnemonic::Fdiv,
                _ => Mnemonic::Fdivr,
            };
            // Destination in operand 1: `fadd st(i), st`.
            Ok(stack(mnemonic, [Operand::None, Operand::Imm8(index), Operand::None]))
        }
        (0xde, 0..=7) => {
            // FCOMPP is the only instruction in the `de d8`-`de df` row, and it
            // is `de d9`: `st(i)` names it in the rm field, so `de d8` really is
            // reserved. This table used to accept `de d0`-`de d7` and reject the
            // real one, which stopped the guest on the `fcompp / fnstsw ax /
            // test ah,0x41` compare idiom.
            if field == 3 {
                return match index {
                    1 => Ok(stack(Mnemonic::Fcompp, [Operand::None; 3])),
                    _ => Err(unsupported(pc, &format!("x87 de /3 rm={index}"))),
                };
            }
            let mnemonic = match field {
                0 => Mnemonic::Faddp,
                1 => Mnemonic::Fmulp,
                2 => return Err(unsupported(pc, "x87 de /2 (DE D0-D7) is reserved")),
                4 => Mnemonic::Fsubp,
                5 => Mnemonic::Fsubrp,
                6 => Mnemonic::Fdivp,
                _ => Mnemonic::Fdivrp,
            };
            Ok(stack(mnemonic, [Operand::None, Operand::Imm8(index), Operand::None]))
        }
        (0xd9, 0) => Ok(stack(Mnemonic::Fld, [Operand::Imm8(index), Operand::None, Operand::None])),
        (0xd9, 1) => Ok(stack(Mnemonic::Fxch, [Operand::Imm8(index), Operand::None, Operand::None])),
        (0xd9, 4) => match index {
            0 => Ok(stack(Mnemonic::Fchs, [Operand::None; 3])),
            1 => Ok(stack(Mnemonic::Fabs, [Operand::None; 3])),
            _ => Err(unsupported(pc, &format!("x87 d9 /4 rm={index}"))),
        },
        (0xd9, 5) => match index {
            0 => Ok(stack(Mnemonic::Fld1, [Operand::None; 3])),
            6 => Ok(stack(Mnemonic::Fldz, [Operand::None; 3])),
            _ => Err(unsupported(pc, &format!("x87 d9 /5 rm={index}"))),
        },
        // `d9 f0`-`d9 f7`: the register field is 6, and the rm field picks the
        // operation. FDECSTP and FINCSTP belong here -- `d9 f6` and `d9 f7`.
        (0xd9, 6) => match index {
            6 => Ok(stack(Mnemonic::Fdecstp, [Operand::None; 3])),
            7 => Ok(stack(Mnemonic::Fincstp, [Operand::None; 3])),
            _ => Err(unsupported(pc, &format!("x87 d9 /6 rm={index}"))),
        },
        // `d9 f8`-`d9 ff`: register field 7. "FSIN is `d9 fe`" -- mapping that
        // slot to FDECSTP, as this table used to, turns every sine the guest
        // computes into a stack rotation and leaves the angle where the sine
        // should be, which draws a linear ramp where an oscillation belongs.
        (0xd9, 7) => match index {
            2 => Ok(stack(Mnemonic::Fsqrt, [Operand::None; 3])),
            6 => Ok(stack(Mnemonic::Fsin, [Operand::None; 3])),
            7 => Ok(stack(Mnemonic::Fcos, [Operand::None; 3])),
            _ => Err(unsupported(pc, &format!("x87 d9 /7 rm={index}"))),
        },
        (0xdb, 4) if index == 2 => Ok(stack(Mnemonic::Fnclex, [Operand::None; 3])),
        (0xdb, 4) if index == 3 => Ok(stack(Mnemonic::Fninit, [Operand::None; 3])),
        (0xdd, 4) => Ok(stack(Mnemonic::Fucom, [Operand::Imm8(index), Operand::None, Operand::None])),
        (0xdd, 5) => Ok(stack(Mnemonic::Fucomp, [Operand::Imm8(index), Operand::None, Operand::None])),
        (0xdf, 4) if index == 0 => Ok(stack(
            Mnemonic::Fnstsw,
            [Operand::Reg(EAX), Operand::None, Operand::None],
        )),
        _ => Err(unsupported(
            pc,
            &format!("x87 register form 0x{opcode:02x} /{field} rm={index}"),
        )),
    }
}

fn decode_opcode(
    cursor: &mut Cursor,
    pc: u32,
    segment: Option<u8>,
    width: Size,
) -> Result<Instruction> {
    let opcode = cursor.next()?;
    let instruction = match opcode {
        0x00..=0x3d if opcode & 0x07 <= 5 => {
            let mnemonic = ALU[usize::from(opcode >> 3)];
            let size = if opcode & 1 == 0 { Size::Byte } else { width };
            match opcode & 0x07 {
                0 | 1 => {
                    let decoded = modrm(cursor, size, segment)?;
                    Instruction::new(
                        pc,
                        cursor.len(),
                        mnemonic,
                        size,
                        [decoded.operand, register_operand(decoded.reg, size), Operand::None],
                    )
                }
                2 | 3 => {
                    let decoded = modrm(cursor, size, segment)?;
                    Instruction::new(
                        pc,
                        cursor.len(),
                        mnemonic,
                        size,
                        [register_operand(decoded.reg, size), decoded.operand, Operand::None],
                    )
                }
                4 => {
                    let immediate = cursor.next()?;
                    Instruction::new(
                        pc,
                        cursor.len(),
                        mnemonic,
                        Size::Byte,
                        [Operand::RegByte(EAX), Operand::Imm8(immediate), Operand::None],
                    )
                }
                _ => {
                    let (size, immediate) = match width {
                        Size::Word => (Size::Word, Operand::Imm16(cursor.u16()?)),
                        _ => (Size::Dword, Operand::Imm32(cursor.u32()?)),
                    };
                    Instruction::new(
                        pc,
                        cursor.len(),
                        mnemonic,
                        size,
                        [register_operand(EAX, size), immediate, Operand::None],
                    )
                }
            }
        }
        0x40..=0x47 => Instruction::new(
            pc,
            cursor.len(),
            Mnemonic::Inc,
            width,
            [
                register_operand(opcode & 7, width),
                Operand::None,
                Operand::None,
            ],
        ),
        0x48..=0x4f => Instruction::new(
            pc,
            cursor.len(),
            Mnemonic::Dec,
            width,
            [
                register_operand(opcode & 7, width),
                Operand::None,
                Operand::None,
            ],
        ),
        0x50..=0x57 => Instruction::new(
            pc,
            cursor.len(),
            Mnemonic::Push,
            width,
            [
                register_operand(opcode & 7, width),
                Operand::None,
                Operand::None,
            ],
        ),
        0x58..=0x5f => Instruction::new(
            pc,
            cursor.len(),
            Mnemonic::Pop,
            width,
            [
                register_operand(opcode & 7, width),
                Operand::None,
                Operand::None,
            ],
        ),
        0x06 | 0x07 | 0x0e | 0x16 | 0x17 | 0x1e | 0x1f => {
            // Segment push and pop. `push cs` (0x0e) has no matching pop because
            // `pop cs` does not exist.
            if width == Size::Word {
                return Err(unsupported(pc, "16-bit segment push or pop"));
            }
            let mnemonic = match opcode {
                0x06 | 0x0e | 0x16 | 0x1e => Mnemonic::PushSeg,
                _ => Mnemonic::PopSeg,
            };
            Instruction::plain(pc, cursor.len(), mnemonic, Size::Dword)
        }
        0x60 | 0x61 => {
            if width == Size::Word {
                return Err(unsupported(pc, "16-bit pushad or popad"));
            }
            let mnemonic = if opcode == 0x60 {
                Mnemonic::Pushad
            } else {
                Mnemonic::Popad
            };
            Instruction::plain(pc, cursor.len(), mnemonic, Size::Dword)
        }
        0x68 => {
            let immediate = match width {
                Size::Word => Operand::Imm16(cursor.u16()?),
                _ => Operand::Imm32(cursor.u32()?),
            };
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Push,
                width,
                [immediate, Operand::None, Operand::None],
            )
        }
        0x69 | 0x6b => {
            let decoded = modrm(cursor, width, segment)?;
            let immediate = if opcode == 0x69 {
                match width {
                    Size::Word => Operand::Imm16(cursor.u16()?),
                    _ => Operand::Imm32(cursor.u32()?),
                }
            } else {
                Operand::Imm8(cursor.next()?)
            };
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Imul,
                width,
                [
                    register_operand(decoded.reg, width),
                    decoded.operand,
                    immediate,
                ],
            )
        }
        0x6a => {
            let immediate = cursor.next()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Push,
                width,
                [Operand::Imm8(immediate), Operand::None, Operand::None],
            )
        }
        0x70..=0x7f => {
            let displacement = cursor.i8()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Jcc(CONDITIONS[usize::from(opcode & 0x0f)]),
                Size::Dword,
                [relative(cursor, i32::from(displacement)), Operand::None, Operand::None],
            )
        }
        0x80 | 0x82 => group1(cursor, pc, Size::Byte, true, segment)?,
        0x81 => group1(cursor, pc, width, false, segment)?,
        0x83 => group1(cursor, pc, width, true, segment)?,
        0x84 | 0x85 => {
            let size = if opcode == 0x84 { Size::Byte } else { width };
            let decoded = modrm(cursor, size, segment)?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Test,
                size,
                [decoded.operand, register_operand(decoded.reg, size), Operand::None],
            )
        }
        0x86 | 0x87 => {
            let size = if opcode == 0x86 { Size::Byte } else { width };
            let decoded = modrm(cursor, size, segment)?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Xchg,
                size,
                [decoded.operand, register_operand(decoded.reg, size), Operand::None],
            )
        }
        0x88..=0x8b => {
            let size = if opcode & 1 == 0 { Size::Byte } else { width };
            let decoded = modrm(cursor, size, segment)?;
            let register = register_operand(decoded.reg, size);
            let operands = if opcode < 0x8a {
                [decoded.operand, register, Operand::None]
            } else {
                [register, decoded.operand, Operand::None]
            };
            Instruction::new(pc, cursor.len(), Mnemonic::Mov, size, operands)
        }
        0x8d => {
            let decoded = modrm(cursor, width, segment)?;
            if decoded.register {
                return Err(unsupported(pc, "lea with a register operand"));
            }
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Lea,
                width,
                [
                    register_operand(decoded.reg, width),
                    decoded.operand,
                    Operand::None,
                ],
            )
        }
        0x8f => {
            let decoded = modrm(cursor, width, segment)?;
            if decoded.reg != 0 {
                return Err(unsupported(pc, "pop with a non-zero reg field"));
            }
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Pop,
                width,
                [decoded.operand, Operand::None, Operand::None],
            )
        }
        0x90 => Instruction::plain(pc, cursor.len(), Mnemonic::Nop, Size::Dword),
        0x91..=0x97 => Instruction::new(
            pc,
            cursor.len(),
            Mnemonic::Xchg,
            width,
            [
                register_operand(EAX, width),
                register_operand(opcode & 7, width),
                Operand::None,
            ],
        ),
        0xd7 => {
            if width == Size::Word {
                return Err(unsupported(pc, "16-bit xlat"));
            }
            Instruction::plain(pc, cursor.len(), Mnemonic::Xlat, Size::Byte)
        }
        0x98 => Instruction::plain(pc, cursor.len(), Mnemonic::Cwde, width),
        0x99 => Instruction::plain(pc, cursor.len(), Mnemonic::Cdq, width),
        0x9c | 0x9d => {
            if width == Size::Word {
                return Err(unsupported(pc, "16-bit pushf or popf"));
            }
            let mnemonic = if opcode == 0x9c {
                Mnemonic::Pushfd
            } else {
                Mnemonic::Popfd
            };
            Instruction::plain(pc, cursor.len(), mnemonic, Size::Dword)
        }
        0x9b => Instruction::plain(pc, cursor.len(), Mnemonic::Wait, Size::Dword),
        0x9e => Instruction::plain(pc, cursor.len(), Mnemonic::Sahf, Size::Dword),
        0x9f => Instruction::plain(pc, cursor.len(), Mnemonic::Lahf, Size::Dword),
        0xa0..=0xa3 => {
            let displacement = cursor.i32()?;
            let address = Operand::Mem(Mem {
                base: None,
                index: None,
                scale: 1,
                disp: displacement,
                segment,
            });
            let size = if opcode & 1 == 0 { Size::Byte } else { width };
            let accumulator = register_operand(EAX, size);
            let operands = if opcode < 0xa2 {
                [accumulator, address, Operand::None]
            } else {
                [address, accumulator, Operand::None]
            };
            Instruction::new(pc, cursor.len(), Mnemonic::Mov, size, operands)
        }
        0xa4 => Instruction::plain(pc, cursor.len(), Mnemonic::Movs, Size::Byte),
        0xa5 => Instruction::plain(pc, cursor.len(), Mnemonic::Movs, width),
        0xa6 => Instruction::plain(pc, cursor.len(), Mnemonic::Cmps, Size::Byte),
        0xa7 => Instruction::plain(pc, cursor.len(), Mnemonic::Cmps, width),
        0xa8 => {
            let immediate = cursor.next()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Test,
                Size::Byte,
                [Operand::RegByte(EAX), Operand::Imm8(immediate), Operand::None],
            )
        }
        0xa9 => {
            let immediate = match width {
                Size::Word => Operand::Imm16(cursor.u16()?),
                _ => Operand::Imm32(cursor.u32()?),
            };
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Test,
                width,
                [register_operand(EAX, width), immediate, Operand::None],
            )
        }
        0xaa => Instruction::plain(pc, cursor.len(), Mnemonic::Stos, Size::Byte),
        0xab => Instruction::plain(pc, cursor.len(), Mnemonic::Stos, width),
        0xac => Instruction::plain(pc, cursor.len(), Mnemonic::Lods, Size::Byte),
        0xad => Instruction::plain(pc, cursor.len(), Mnemonic::Lods, width),
        0xae => Instruction::plain(pc, cursor.len(), Mnemonic::Scas, Size::Byte),
        0xaf => Instruction::plain(pc, cursor.len(), Mnemonic::Scas, width),
        0xb0..=0xb7 => {
            let immediate = cursor.next()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Mov,
                Size::Byte,
                [byte_operand(opcode & 7), Operand::Imm8(immediate), Operand::None],
            )
        }
        0xb8..=0xbf => {
            let immediate = match width {
                Size::Word => Operand::Imm16(cursor.u16()?),
                _ => Operand::Imm32(cursor.u32()?),
            };
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Mov,
                width,
                [
                    register_operand(opcode & 7, width),
                    immediate,
                    Operand::None,
                ],
            )
        }
        0xc0 => group2(cursor, pc, Size::Byte, Count::Immediate, segment)?,
        0xc1 => group2(cursor, pc, width, Count::Immediate, segment)?,
        0xc2 => {
            let count = cursor.u16()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Ret,
                Size::Dword,
                [
                    Operand::Imm32(u32::from(count)),
                    Operand::None,
                    Operand::None,
                ],
            )
        }
        0xc3 => Instruction::plain(pc, cursor.len(), Mnemonic::Ret, Size::Dword),
        0xc6 | 0xc7 => {
            let size = if opcode == 0xc6 { Size::Byte } else { width };
            let decoded = modrm(cursor, size, segment)?;
            if decoded.reg != 0 {
                return Err(unsupported(pc, "mov with a non-zero reg field"));
            }
            let immediate = match size {
                Size::Byte => Operand::Imm8(cursor.next()?),
                Size::Word => Operand::Imm16(cursor.u16()?),
                Size::Dword => Operand::Imm32(cursor.u32()?),
            };
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Mov,
                size,
                [decoded.operand, immediate, Operand::None],
            )
        }
        0xc9 => Instruction::plain(pc, cursor.len(), Mnemonic::Leave, Size::Dword),
        0xcc => Instruction::plain(pc, cursor.len(), Mnemonic::Int3, Size::Dword),
        0xd0 => group2(cursor, pc, Size::Byte, Count::One, segment)?,
        0xd1 => group2(cursor, pc, width, Count::One, segment)?,
        0xd2 => group2(cursor, pc, Size::Byte, Count::Cl, segment)?,
        0xd3 => group2(cursor, pc, width, Count::Cl, segment)?,
        0xd8..=0xdf => x87(cursor, pc, opcode)?,
        0xe8 => {
            let displacement = cursor.i32()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Call,
                Size::Dword,
                [relative(cursor, displacement), Operand::None, Operand::None],
            )
        }
        0xe9 => {
            let displacement = cursor.i32()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Jmp,
                Size::Dword,
                [relative(cursor, displacement), Operand::None, Operand::None],
            )
        }
        0xeb => {
            let displacement = cursor.i8()?;
            Instruction::new(
                pc,
                cursor.len(),
                Mnemonic::Jmp,
                Size::Dword,
                [relative(cursor, i32::from(displacement)), Operand::None, Operand::None],
            )
        }
        0xe0..=0xe3 => {
            let displacement = cursor.i8()?;
            let mnemonic = match opcode {
                0xe0 => Mnemonic::Loopne,
                0xe1 => Mnemonic::Loope,
                0xe2 => Mnemonic::Loop,
                _ => Mnemonic::Jecxz,
            };
            Instruction::new(
                pc,
                cursor.len(),
                mnemonic,
                Size::Dword,
                [
                    relative(cursor, i32::from(displacement)),
                    Operand::None,
                    Operand::None,
                ],
            )
        }
        0xf4 => Instruction::plain(pc, cursor.len(), Mnemonic::Hlt, Size::Dword),
        0xf5 => Instruction::plain(pc, cursor.len(), Mnemonic::Cmc, Size::Dword),
        0xf8 => Instruction::plain(pc, cursor.len(), Mnemonic::Clc, Size::Dword),
        0xf9 => Instruction::plain(pc, cursor.len(), Mnemonic::Stc, Size::Dword),
        0xfa => Instruction::plain(pc, cursor.len(), Mnemonic::Cli, Size::Dword),
        0xfb => Instruction::plain(pc, cursor.len(), Mnemonic::Sti, Size::Dword),
        0xfc => Instruction::plain(pc, cursor.len(), Mnemonic::Cld, Size::Dword),
        0xfd => Instruction::plain(pc, cursor.len(), Mnemonic::Std, Size::Dword),
        0xf6 => group3(cursor, pc, Size::Byte, segment)?,
        0xf7 => group3(cursor, pc, width, segment)?,
        0xfe => {
            let decoded = modrm(cursor, Size::Byte, segment)?;
            let mnemonic = match decoded.reg {
                0 => Mnemonic::Inc,
                1 => Mnemonic::Dec,
                _ => return Err(unsupported(pc, "group 4 with an unknown reg field")),
            };
            Instruction::new(
                pc,
                cursor.len(),
                mnemonic,
                Size::Byte,
                [decoded.operand, Operand::None, Operand::None],
            )
        }
        0xff => {
            let decoded = modrm(cursor, width, segment)?;
            let mnemonic = match decoded.reg {
                0 => Mnemonic::Inc,
                1 => Mnemonic::Dec,
                2 | 4 if width == Size::Word => {
                    return Err(unsupported(pc, "16-bit near call or jump"));
                }
                2 => Mnemonic::Call,
                4 => Mnemonic::Jmp,
                6 => Mnemonic::Push,
                3 | 5 => return Err(unsupported(pc, "far call or far jump")),
                _ => return Err(unsupported(pc, "opcode 0xff with a reserved reg field")),
            };
            Instruction::new(
                pc,
                cursor.len(),
                mnemonic,
                width,
                [decoded.operand, Operand::None, Operand::None],
            )
        }
        0x0f => two_byte(cursor, pc, segment, width)?,
        _ => return Err(unsupported(pc, &format!("opcode 0x{opcode:02x}"))),
    };
    Ok(instruction)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{EAX, EBP, EBX, ECX, EDI, EDX, ESP};
    use crate::pe::IMAGE_BASE;

    fn decode_bytes(bytes: &[u8]) -> Instruction {
        let mut memory = GuestMemory::empty(0x100).unwrap();
        memory.write_bytes(IMAGE_BASE, bytes).unwrap();
        decode(&memory, IMAGE_BASE).expect("decodes")
    }

    fn decode_failure(bytes: &[u8]) -> String {
        let mut memory = GuestMemory::empty(0x100).unwrap();
        memory.write_bytes(IMAGE_BASE, bytes).unwrap();
        decode(&memory, IMAGE_BASE).expect_err("must not decode")
    }

    #[test]
    fn decodes_the_alu_family_in_both_widths() {
        let add = decode_bytes(&[0x01, 0xc8]);
        assert_eq!(add.mnemonic, Mnemonic::Add);
        assert_eq!(add.size, Size::Dword);
        assert_eq!(add.operands[0], Operand::Reg(EAX));
        assert_eq!(add.operands[1], Operand::Reg(ECX));
        assert_eq!(add.len, 2);

        // 00 /r is the r/m8, r8 direction: modrm d8 has rm=eax and reg=ebx, so
        // eax is the destination and ebx the source.
        let add8 = decode_bytes(&[0x00, 0xd8]);
        assert_eq!(add8.size, Size::Byte);
        assert_eq!(add8.operands[0], Operand::RegByte(EAX));
        assert_eq!(add8.operands[1], Operand::RegByte(EBX));

        let cmp_al = decode_bytes(&[0x3c, 0x7f]);
        assert_eq!(cmp_al.mnemonic, Mnemonic::Cmp);
        assert_eq!(cmp_al.operands[0], Operand::RegByte(EAX));
        assert_eq!(cmp_al.operands[1], Operand::Imm8(0x7f));

        let xor_eax = decode_bytes(&[0x35, 0x78, 0x56, 0x34, 0x12]);
        assert_eq!(xor_eax.mnemonic, Mnemonic::Xor);
        assert_eq!(xor_eax.operands[1], Operand::Imm32(0x1234_5678));
        assert_eq!(xor_eax.len, 5);

        // The 8-bit register-to-memory direction.
        let or_mem = decode_bytes(&[0x08, 0x04, 0x24]);
        assert_eq!(or_mem.mnemonic, Mnemonic::Or);
        assert_eq!(or_mem.operands[0], Operand::Mem(Mem { base: Some(4), index: None, scale: 1, disp: 0, segment: None }));
        assert_eq!(or_mem.operands[1], Operand::RegByte(EAX));
    }

    #[test]
    fn decodes_group_one_immediates() {
        let add = decode_bytes(&[0x83, 0xc0, 0x05]);
        assert_eq!(add.mnemonic, Mnemonic::Add);
        assert_eq!(add.operands[0], Operand::Reg(EAX));
        assert_eq!(add.operands[1], Operand::Imm8(5));
        assert_eq!(add.len, 3);

        let sub = decode_bytes(&[0x81, 0xec, 0x00, 0x01, 0x00, 0x00]);
        assert_eq!(sub.mnemonic, Mnemonic::Sub);
        assert_eq!(sub.operands[0], Operand::Reg(ESP));
        assert_eq!(sub.operands[1], Operand::Imm32(0x100));
        assert_eq!(sub.len, 6);

        let byte = decode_bytes(&[0x80, 0x3d, 0x00, 0x20, 0x40, 0x00, 0x01]);
        assert_eq!(byte.mnemonic, Mnemonic::Cmp);
        assert_eq!(byte.size, Size::Byte);
        assert_eq!(byte.len, 7);
    }

    #[test]
    fn decodes_stack_instructions() {
        assert_eq!(decode_bytes(&[0x40]).mnemonic, Mnemonic::Inc);
        assert_eq!(decode_bytes(&[0x4f]).operands[0], Operand::Reg(EDI));
        assert_eq!(decode_bytes(&[0x50]).operands[0], Operand::Reg(EAX));
        assert_eq!(decode_bytes(&[0x5d]).operands[0], Operand::Reg(EBP));
        assert_eq!(decode_bytes(&[0x06]).mnemonic, Mnemonic::PushSeg);
        assert_eq!(decode_bytes(&[0x07]).mnemonic, Mnemonic::PopSeg);
        assert_eq!(decode_bytes(&[0x60]).mnemonic, Mnemonic::Pushad);
        assert_eq!(decode_bytes(&[0x61]).mnemonic, Mnemonic::Popad);

        let push = decode_bytes(&[0x68, 0x00, 0x10, 0x40, 0x00]);
        assert_eq!(push.operands[0], Operand::Imm32(0x0040_1000));
        assert_eq!(push.len, 5);
        assert_eq!(decode_bytes(&[0x6a, 0xff]).operands[0], Operand::Imm8(0xff));
        assert_eq!(decode_bytes(&[0x8f, 0x44, 0x24, 0x04]).mnemonic, Mnemonic::Pop);
    }

    #[test]
    fn resolves_relative_branches_to_addresses() {
        let je = decode_bytes(&[0x74, 0x05]);
        assert_eq!(je.mnemonic, Mnemonic::Jcc(Condition::Equal));
        assert_eq!(je.operands[0], Operand::Imm32(IMAGE_BASE + 7));

        let short_loop = decode_bytes(&[0xeb, 0xfe]);
        assert_eq!(short_loop.operands[0], Operand::Imm32(IMAGE_BASE));

        let call = decode_bytes(&[0xe8, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(call.mnemonic, Mnemonic::Call);
        assert_eq!(call.operands[0], Operand::Imm32(IMAGE_BASE + 5));

        let jump_back = decode_bytes(&[0xe9, 0xfb, 0xff, 0xff, 0xff]);
        assert_eq!(jump_back.operands[0], Operand::Imm32(IMAGE_BASE));

        let long_je = decode_bytes(&[0x0f, 0x84, 0x10, 0x00, 0x00, 0x00]);
        assert_eq!(long_je.mnemonic, Mnemonic::Jcc(Condition::Equal));
        assert_eq!(long_je.operands[0], Operand::Imm32(IMAGE_BASE + 22));
        assert_eq!(long_je.len, 6);
    }

    #[test]
    fn decodes_modrm_addressing_modes() {
        let base_disp = decode_bytes(&[0x8b, 0x44, 0x24, 0x04]);
        assert_eq!(
            base_disp.operands[1],
            Operand::Mem(Mem { base: Some(ESP), index: None, scale: 1, disp: 4, segment: None })
        );

        let scaled = decode_bytes(&[0x8b, 0x04, 0x85, 0x00, 0x10, 0x40, 0x00]);
        assert_eq!(
            scaled.operands[1],
            Operand::Mem(Mem { base: None, index: Some(EAX), scale: 4, disp: 0x0040_1000, segment: None })
        );

        let absolute = decode_bytes(&[0x8b, 0x05, 0x34, 0x12, 0x40, 0x00]);
        assert_eq!(
            absolute.operands[1],
            Operand::Mem(Mem { base: None, index: None, scale: 1, disp: 0x0040_1234, segment: None })
        );

        let disp8 = decode_bytes(&[0x8b, 0x4d, 0xf8]);
        assert_eq!(
            disp8.operands[0],
            Operand::Reg(ECX)
        );
        assert_eq!(
            disp8.operands[1],
            Operand::Mem(Mem { base: Some(EBP), index: None, scale: 1, disp: -8, segment: None })
        );

        // SIB 0xd5: scale 8, index edx, no base, absolute displacement.
        let two_registers = decode_bytes(&[0x8b, 0x14, 0xd5, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            two_registers.operands[1],
            Operand::Mem(Mem { base: None, index: Some(EDX), scale: 8, disp: 0, segment: None })
        );
    }

    #[test]
    fn decodes_segment_overrides() {
        let fs_read = decode_bytes(&[0x64, 0xa1, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(fs_read.mnemonic, Mnemonic::Mov);
        assert_eq!(
            fs_read.operands[1],
            Operand::Mem(Mem { base: None, index: None, scale: 1, disp: 0, segment: Some(0x64) })
        );
        assert_eq!(fs_read.len, 6);

        let fs_generic = decode_bytes(&[0x64, 0x8b, 0x0d, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(
            fs_generic.operands[1],
            Operand::Mem(Mem { base: None, index: None, scale: 1, disp: 0, segment: Some(0x64) })
        );
    }

    #[test]
    fn decodes_moves_and_lea() {
        let byte_move = decode_bytes(&[0x88, 0xd8]);
        assert_eq!(byte_move.operands[0], Operand::RegByte(EAX));
        assert_eq!(byte_move.operands[1], Operand::RegByte(EBX));
        assert_eq!(decode_bytes(&[0x89, 0xd8]).mnemonic, Mnemonic::Mov);
        assert_eq!(
            decode_bytes(&[0x8a, 0xc4]).operands[1],
            Operand::HighByte(0),
            "mov al, ah"
        );

        let immediate = decode_bytes(&[0xb8, 0x00, 0x00, 0x00, 0x00]);
        assert_eq!(immediate.operands[0], Operand::Reg(EAX));
        assert_eq!(immediate.operands[1], Operand::Imm32(0));
        assert_eq!(immediate.len, 5);
        assert_eq!(decode_bytes(&[0xb4, 0x10]).operands[0], Operand::HighByte(0));

        let lea = decode_bytes(&[0x8d, 0x44, 0x24, 0x08]);
        assert_eq!(lea.mnemonic, Mnemonic::Lea);
        assert_eq!(lea.operands[0], Operand::Reg(EAX));

        let moffs = decode_bytes(&[0xa3, 0x00, 0x00, 0x4c, 0x00]);
        assert_eq!(moffs.mnemonic, Mnemonic::Mov);
        assert_eq!(moffs.operands[1], Operand::Reg(EAX));
        assert_eq!(moffs.len, 5);

        let mov_imm_mem = decode_bytes(&[0xc7, 0x05, 0x00, 0x00, 0x4c, 0x00, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(mov_imm_mem.operands[1], Operand::Imm32(1));
        assert_eq!(mov_imm_mem.len, 10);
    }

    #[test]
    fn decodes_shifts_and_group_three() {
        let shl = decode_bytes(&[0xd1, 0xe0]);
        assert_eq!(shl.mnemonic, Mnemonic::Shl);
        assert_eq!(shl.operands[1], Operand::Imm8(1));

        let sar = decode_bytes(&[0xd3, 0xf8]);
        assert_eq!(sar.mnemonic, Mnemonic::Sar);
        assert_eq!(sar.operands[1], Operand::RegByte(ECX));

        let rol = decode_bytes(&[0xc0, 0xc0, 0x04]);
        assert_eq!(rol.mnemonic, Mnemonic::Rol);
        assert_eq!(rol.operands[0], Operand::RegByte(EAX));

        let neg = decode_bytes(&[0xf7, 0xd8]);
        assert_eq!(neg.mnemonic, Mnemonic::Neg);
        let div = decode_bytes(&[0xf7, 0xf1]);
        assert_eq!(div.mnemonic, Mnemonic::Div);
        let test = decode_bytes(&[0xf7, 0xc0, 0x01, 0x00, 0x00, 0x00]);
        assert_eq!(test.mnemonic, Mnemonic::Test);
        assert_eq!(test.operands[1], Operand::Imm32(1));
        assert_eq!(test.len, 6);
    }

    #[test]
    fn decodes_group_five_including_iat_calls() {
        let call = decode_bytes(&[0xff, 0x15, 0x00, 0x90, 0x4b, 0x00]);
        assert_eq!(call.mnemonic, Mnemonic::Call);
        assert_eq!(
            call.operands[0],
            Operand::Mem(Mem { base: None, index: None, scale: 1, disp: 0x004b_9000, segment: None })
        );
        assert_eq!(call.len, 6);

        assert_eq!(decode_bytes(&[0xff, 0xe0]).mnemonic, Mnemonic::Jmp);
        assert_eq!(decode_bytes(&[0xff, 0x75, 0x08]).mnemonic, Mnemonic::Push);
        assert_eq!(decode_bytes(&[0xfe, 0xc0]).mnemonic, Mnemonic::Inc);
        assert_eq!(decode_bytes(&[0xfe, 0xc8]).size, Size::Byte);
        assert_eq!(
            decode_bytes(&[0xff, 0x00]).mnemonic,
            Mnemonic::Inc,
            "increment through memory"
        );
    }

    #[test]
    fn decodes_the_two_byte_map() {
        let cmov = decode_bytes(&[0x0f, 0x45, 0xc1]);
        assert_eq!(cmov.mnemonic, Mnemonic::Cmovcc(Condition::NotEqual));
        assert_eq!(cmov.operands[0], Operand::Reg(EAX));
        assert_eq!(cmov.operands[1], Operand::Reg(ECX));

        let setcc = decode_bytes(&[0x0f, 0x94, 0xc0]);
        assert_eq!(setcc.mnemonic, Mnemonic::Setcc(Condition::Equal));
        assert_eq!(setcc.operands[0], Operand::RegByte(EAX));
        assert_eq!(setcc.size, Size::Byte);

        let movzx = decode_bytes(&[0x0f, 0xb6, 0xc1]);
        assert_eq!(movzx.mnemonic, Mnemonic::Movzx);
        assert_eq!(movzx.size, Size::Byte);
        assert_eq!(movzx.operands[0], Operand::Reg(EAX));

        let movsx = decode_bytes(&[0x0f, 0xbe, 0x44, 0x24, 0x04]);
        assert_eq!(movsx.mnemonic, Mnemonic::Movsx);
        assert_eq!(movsx.operands[1], Operand::Mem(Mem { base: Some(ESP), index: None, scale: 1, disp: 4, segment: None }));

        let imul = decode_bytes(&[0x0f, 0xaf, 0xc1]);
        assert_eq!(imul.mnemonic, Mnemonic::Imul);
        assert_eq!(imul.operands[0], Operand::Reg(EAX));

        let shld = decode_bytes(&[0x0f, 0xa4, 0xc8, 0x05]);
        assert_eq!(shld.mnemonic, Mnemonic::Shld);
        assert_eq!(shld.operands[2], Operand::Imm8(5));

        let shrd = decode_bytes(&[0x0f, 0xad, 0xc8]);
        assert_eq!(shrd.mnemonic, Mnemonic::Shrd);
        assert_eq!(shrd.operands[2], Operand::RegByte(ECX));

        let nop = decode_bytes(&[0x0f, 0x1f, 0x00]);
        assert_eq!(nop.mnemonic, Mnemonic::Nop);
        assert_eq!(nop.len, 3);
    }

    #[test]
    fn decodes_three_operand_imul() {
        let imul = decode_bytes(&[0x6b, 0xc1, 0x04]);
        assert_eq!(imul.mnemonic, Mnemonic::Imul);
        assert_eq!(imul.operands[0], Operand::Reg(EAX));
        assert_eq!(imul.operands[1], Operand::Reg(ECX));
        assert_eq!(imul.operands[2], Operand::Imm8(4));

        let wide = decode_bytes(&[0x69, 0xc1, 0x00, 0x01, 0x00, 0x00]);
        assert_eq!(wide.operands[2], Operand::Imm32(0x100));
    }

    #[test]
    fn decodes_miscellaneous_instructions() {
        assert_eq!(decode_bytes(&[0x90]).mnemonic, Mnemonic::Nop);
        assert_eq!(decode_bytes(&[0x91]).mnemonic, Mnemonic::Xchg);
        assert_eq!(decode_bytes(&[0xd7]).mnemonic, Mnemonic::Xlat);
        assert_eq!(decode_bytes(&[0x98]).mnemonic, Mnemonic::Cwde);
        assert_eq!(decode_bytes(&[0x99]).mnemonic, Mnemonic::Cdq);
        // The `0x66` prefix selects the 16-bit form of each of them.
        assert_eq!(decode_bytes(&[0x66, 0x98]).size, Size::Word);
        assert_eq!(decode_bytes(&[0x66, 0x99]).size, Size::Word);
        assert_eq!(decode_bytes(&[0x9c]).mnemonic, Mnemonic::Pushfd);
        assert_eq!(decode_bytes(&[0x9d]).mnemonic, Mnemonic::Popfd);
        assert_eq!(decode_bytes(&[0x9e]).mnemonic, Mnemonic::Sahf);
        assert_eq!(decode_bytes(&[0x9f]).mnemonic, Mnemonic::Lahf);
        assert_eq!(decode_bytes(&[0xc9]).mnemonic, Mnemonic::Leave);
        assert_eq!(decode_bytes(&[0xcc]).mnemonic, Mnemonic::Int3);
        assert_eq!(decode_bytes(&[0xf4]).mnemonic, Mnemonic::Hlt);
        assert_eq!(decode_bytes(&[0xc3]).mnemonic, Mnemonic::Ret);

        let ret = decode_bytes(&[0xc2, 0x08, 0x00]);
        assert_eq!(ret.mnemonic, Mnemonic::Ret);
        assert_eq!(ret.operands[0], Operand::Imm32(8));
        assert_eq!(ret.len, 3, "ret imm16 carries a 16-bit immediate");
    }

    #[test]
    fn decodes_string_instructions_with_repeats() {
        let movsb = decode_bytes(&[0xf3, 0xa4]);
        assert_eq!(movsb.mnemonic, Mnemonic::Movs);
        assert_eq!(movsb.size, Size::Byte);
        assert_eq!(movsb.repeat, Repeat::WhileZero);
        assert_eq!(movsb.len, 2);

        let scas = decode_bytes(&[0xf2, 0xae]);
        assert_eq!(scas.mnemonic, Mnemonic::Scas);
        assert_eq!(scas.repeat, Repeat::WhileNotZero);

        assert_eq!(decode_bytes(&[0xa5]).size, Size::Dword);
        assert_eq!(decode_bytes(&[0xaa]).mnemonic, Mnemonic::Stos);
        assert_eq!(decode_bytes(&[0xad]).mnemonic, Mnemonic::Lods);
        assert_eq!(decode_bytes(&[0xa7]).mnemonic, Mnemonic::Cmps);

        // A rep prefix on a non-string instruction is carried but harmless;
        // pause is the one the compiler emits.
        let pause = decode_bytes(&[0xf3, 0x90]);
        assert_eq!(pause.mnemonic, Mnemonic::Nop);
    }

    #[test]
    fn decodes_the_x87_forms_the_crt_uses() {
        let load = decode_bytes(&[0xdd, 0x05, 0x78, 0xbb, 0x4b, 0x00]);
        assert_eq!(load.mnemonic, Mnemonic::Fld);
        assert!(load.double, "fld qword is a 64-bit load");
        assert_eq!(load.len, 6);
        assert_eq!(
            load.operands[0],
            Operand::Mem(Mem::direct(0x004b_bb78))
        );

        let divide = decode_bytes(&[0xdc, 0x75, 0xf8]);
        assert_eq!(divide.mnemonic, Mnemonic::Fdiv);
        assert!(divide.double);
        assert_eq!(
            divide.operands[0],
            Operand::Mem(Mem {
                base: Some(EBP),
                index: None,
                scale: 1,
                disp: -8,
                segment: None,
            })
        );

        let compare = decode_bytes(&[0xdc, 0x1d, 0x30, 0x93, 0x4b, 0x00]);
        assert_eq!(compare.mnemonic, Mnemonic::Fcomp);
        assert!(compare.double);

        let status = decode_bytes(&[0xdf, 0xe0]);
        assert_eq!(status.mnemonic, Mnemonic::Fnstsw);
        assert_eq!(status.operands[0], Operand::Reg(EAX));

        assert_eq!(decode_bytes(&[0x9b]).mnemonic, Mnemonic::Wait);
    }

    #[test]
    fn decodes_sixteen_bit_operand_size() {
        // mov ax, bx
        let mov = decode_bytes(&[0x66, 0x89, 0xd8]);
        assert_eq!(mov.mnemonic, Mnemonic::Mov);
        assert_eq!(mov.size, Size::Word);
        assert_eq!(mov.operands[0], Operand::Reg16(EAX));
        assert_eq!(mov.operands[1], Operand::Reg16(EBX));
        assert_eq!(mov.len, 3);

        // mov ax, [ebp+4]
        let load = decode_bytes(&[0x66, 0x8b, 0x45, 0x04]);
        assert_eq!(load.operands[0], Operand::Reg16(EAX));
        assert_eq!(
            load.operands[1],
            Operand::Mem(Mem { base: Some(EBP), index: None, scale: 1, disp: 4, segment: None })
        );

        // mov ax, 0x1234
        let immediate = decode_bytes(&[0x66, 0xb8, 0x34, 0x12]);
        assert_eq!(immediate.operands[0], Operand::Reg16(EAX));
        assert_eq!(immediate.operands[1], Operand::Imm16(0x1234));
        assert_eq!(immediate.len, 4);

        // add ax, 0x1234
        let add = decode_bytes(&[0x66, 0x81, 0xc0, 0x34, 0x12]);
        assert_eq!(add.mnemonic, Mnemonic::Add);
        assert_eq!(add.size, Size::Word);
        assert_eq!(add.operands[0], Operand::Reg16(EAX));
        assert_eq!(add.operands[1], Operand::Imm16(0x1234));
        assert_eq!(add.len, 5);

        // add ax, 5 keeps the sign-extended byte form.
        let short = decode_bytes(&[0x66, 0x83, 0xc0, 0x05]);
        assert_eq!(short.size, Size::Word);
        assert_eq!(short.operands[1], Operand::Imm8(5));

        // The opcode-embedded registers follow the width too.
        let inc = decode_bytes(&[0x66, 0x40]);
        assert_eq!(inc.size, Size::Word);
        assert_eq!(inc.operands[0], Operand::Reg16(EAX));
        let push = decode_bytes(&[0x66, 0x50]);
        assert_eq!(push.size, Size::Word);
        assert_eq!(push.operands[0], Operand::Reg16(EAX));
        let pop = decode_bytes(&[0x66, 0x8f, 0x45, 0x04]);
        assert_eq!(pop.mnemonic, Mnemonic::Pop);
        assert_eq!(pop.size, Size::Word);

        // movzx eax, word ptr [ecx] and movsx eax, word ptr [ecx].
        let movzx = decode_bytes(&[0x0f, 0xb7, 0x01]);
        assert_eq!(movzx.mnemonic, Mnemonic::Movzx);
        assert_eq!(movzx.size, Size::Word);
        assert_eq!(movzx.operands[0], Operand::Reg(EAX));
        assert_eq!(
            movzx.operands[1],
            Operand::Mem(Mem { base: Some(ECX), index: None, scale: 1, disp: 0, segment: None })
        );

        let movsx = decode_bytes(&[0x0f, 0xbf, 0xc1]);
        assert_eq!(movsx.mnemonic, Mnemonic::Movsx);
        assert_eq!(movsx.size, Size::Word);
        assert_eq!(movsx.operands[1], Operand::Reg16(ECX));

        // rep stosw
        let stosw = decode_bytes(&[0xf3, 0x66, 0xab]);
        assert_eq!(stosw.mnemonic, Mnemonic::Stos);
        assert_eq!(stosw.size, Size::Word);
        assert_eq!(stosw.repeat, Repeat::WhileZero);
        assert_eq!(stosw.len, 3);

        // A segment override composes with the width prefix in either order.
        let fs = decode_bytes(&[0x64, 0x66, 0x8b, 0x00]);
        assert_eq!(fs.size, Size::Word);
        assert_eq!(
            fs.operands[1],
            Operand::Mem(Mem { base: Some(EAX), index: None, scale: 1, disp: 0, segment: Some(0x64) })
        );
        assert_eq!(fs.len, 4);
    }

    #[test]
    fn rejects_encodings_it_cannot_execute() {
        assert!(decode_failure(&[0x67, 0x8b, 0x00]).contains("16-bit addressing"));
        assert!(decode_failure(&[0x0f, 0x38, 0x00]).contains("0x0f 0x38"));
        assert!(decode_failure(&[0x66, 0xff, 0xd0]).contains("16-bit near call"));
        assert!(decode_failure(&[0xff, 0x18]).contains("far"));
        assert!(decode_failure(&[0x8d, 0xc0]).contains("lea"));
    }

    #[test]
    fn a_truncated_instruction_is_an_error_not_a_short_read() {
        // `mov dword ptr [0x401000], 1` needs ten bytes; only two are mapped.
        let mut memory = GuestMemory::empty(2).unwrap();
        memory.write_bytes(IMAGE_BASE, &[0xc7, 0x05]).unwrap();
        let error = decode(&memory, IMAGE_BASE).expect_err("must not decode");
        assert!(error.contains("not mapped"), "unexpected error: {error}");
    }

    #[test]
    fn a_truncated_word_immediate_is_an_error() {
        let mut memory = GuestMemory::empty(2).unwrap();
        memory.write_bytes(IMAGE_BASE, &[0x66, 0xb8]).unwrap();
        let error = decode(&memory, IMAGE_BASE).expect_err("must not decode");
        assert!(error.contains("not mapped"), "unexpected error: {error}");
    }

    #[test]
    fn the_integer_memory_forms_of_the_x87_arithmetic_decode() {
        // `da /1` is FIMUL m32int and `de /0` is FIADD m16int: the same arithmetic
        // as their float siblings with the operand read as a signed integer.
        let multiply = decode_bytes(&[0xda, 0x0d, 0x00, 0x20, 0x00, 0x00]);
        assert_eq!(multiply.mnemonic, Mnemonic::Fmul);
        assert!(multiply.integer && multiply.x87);
        assert_eq!(multiply.size, Size::Dword);

        let add = decode_bytes(&[0xde, 0x05, 0x00, 0x20, 0x00, 0x00]);
        assert_eq!(add.mnemonic, Mnemonic::Fadd);
        assert!(add.integer);
        assert_eq!(add.size, Size::Word);
    }

    #[test]
    fn the_instruction_the_board_faulted_on_decodes_from_the_real_image() {
        // `0x0040747b` in the guest is `da 88 d8 02 00 00`: FIMUL against the
        // dword at [eax+0x2d8]. This test exists because a stale page reported
        // this exact address as unsupported long after the decoder learned it.
        let Some(image) = crate::reference_image() else {
            return;
        };
        let memory = GuestMemory::load(image).expect("image maps");
        let instruction = decode(&memory, 0x0040_747b).expect("the board's fimul decodes");
        assert_eq!(instruction.mnemonic, Mnemonic::Fmul);
        assert!(instruction.integer, "its operand is an integer, not a float");
        assert_eq!(instruction.size, Size::Dword);
    }

    #[test]
    fn the_x87_compare_and_pop_twice_is_de_d9() {
        // `de d9` is FCOMPP; `de d8` is reserved, and the decoder used to swap
        // the two, which stopped the guest on its float compare idiom.
        let compare = decode_bytes(&[0xde, 0xd9]);
        assert_eq!(compare.mnemonic, Mnemonic::Fcompp);
        assert!(compare.x87);
        assert!(decode_failure(&[0xde, 0xd8]).contains("rm=0"), "de d8 is not an instruction");
    }

    #[test]
    fn the_x87_sine_and_cosine_are_not_stack_rotations() {
        // `d9 fe` is FSIN and `d9 ff` is FCOS. They sit next to FDECSTP (`d9 f6`)
        // and FINCSTP (`d9 f7`) in the table, and confusing them is silent: the
        // angle stays on the stack and every animated curve in the guest is
        // drawn as a straight ramp instead of an oscillation.
        assert_eq!(decode_bytes(&[0xd9, 0xfe]).mnemonic, Mnemonic::Fsin);
        assert_eq!(decode_bytes(&[0xd9, 0xff]).mnemonic, Mnemonic::Fcos);
        assert_eq!(decode_bytes(&[0xd9, 0xf6]).mnemonic, Mnemonic::Fdecstp);
        assert_eq!(decode_bytes(&[0xd9, 0xf7]).mnemonic, Mnemonic::Fincstp);
        assert!(decode_bytes(&[0xd9, 0xfe]).x87, "fsin runs on the x87 path");
    }

    #[test]
    fn prefix_floods_are_rejected() {
        // A repeated prefix is legal, if pointless: the last one wins.
        assert_eq!(decode_bytes(&[0x66, 0x66, 0xb8, 0x34, 0x12]).size, Size::Word);
        let error = decode_failure(&[0x66, 0x66, 0x66, 0x66, 0x66, 0x90]);
        assert!(error.contains("prefixes"), "unexpected error: {error}");
        let error = decode_failure(&[0xf3, 0xf3, 0xf3, 0xf3, 0xf3, 0x90]);
        assert!(error.contains("prefixes"), "unexpected error: {error}");
    }
}
