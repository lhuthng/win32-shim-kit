//! Executes one decoded instruction.
//!
//! The decoder has already resolved relative targets and advanced the program
//! counter, so everything here works with absolute addresses. Flags are computed
//! eagerly and exactly for each width, because the guest's own comparisons and
//! the CRT's string routines depend on them.
//!
//! Two kinds of stop are not instructions the guest can control: `hlt` ends the
//! run, and a call through an import slot hands control to the host. Everything
//! else either continues or faults with an error naming the address, so a bad
//! instruction can never be mistaken for progress.

use crate::cpu::{
    Condition, Cpu, Flags, HostCall, Instruction, Mem, Mnemonic, Operand, Repeat, Size, Step, EAX,
    ECX, EDX, SEG_FS,
};
use crate::memory::{GuestMemory, TEB_BASE};
use crate::Result;

/// The arithmetic and logic family, sharing one flag implementation.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Alu {
    Add,
    Or,
    Adc,
    Sbb,
    And,
    Sub,
    Xor,
    Cmp,
    Test,
}

fn mask_of(size: Size) -> u32 {
    match size {
        Size::Byte => 0xff,
        Size::Word => 0xffff,
        Size::Dword => 0xffff_ffff,
    }
}

fn sign_of(size: Size) -> u32 {
    match size {
        Size::Byte => 0x80,
        Size::Word => 0x8000,
        Size::Dword => 0x8000_0000,
    }
}

fn bits_of(size: Size) -> u32 {
    size.bytes() * 8
}

/// Sign-extends the low `size` bytes of `value` to a 32-bit signed value.
fn sign_extend(value: u32, size: Size) -> i32 {
    match size {
        Size::Byte => value as u8 as i8 as i32,
        Size::Word => value as u16 as i16 as i32,
        Size::Dword => value as i32,
    }
}

fn address_of(cpu: &Cpu, memory: Mem) -> u32 {
    let mut address = memory.disp as u32;
    if let Some(base) = memory.base {
        address = address.wrapping_add(cpu.regs.get(base));
    }
    if let Some(index) = memory.index {
        address = address.wrapping_add(cpu.regs.get(index).wrapping_mul(u32::from(memory.scale)));
    }
    if memory.segment == Some(SEG_FS) {
        address = address.wrapping_add(TEB_BASE);
    }
    address
}

fn read_operand(memory: &GuestMemory, cpu: &Cpu, operand: Operand, size: Size) -> Result<u32> {
    match operand {
        Operand::Reg(register) => Ok(cpu.regs.get(register)),
        Operand::Reg16(register) => Ok(u32::from(cpu.regs.get_word(register))),
        Operand::RegByte(register) => Ok(u32::from(cpu.regs.get_byte(register))),
        Operand::HighByte(register) => Ok(u32::from(cpu.regs.get_high_byte(register))),
        Operand::Mem(target) => {
            let address = address_of(cpu, target);
            match size {
                Size::Byte => Ok(u32::from(memory.read_u8(address)?)),
                Size::Word => Ok(u32::from(memory.read_u16(address)?)),
                Size::Dword => memory.read_u32(address),
            }
        }
        Operand::Imm8(value) => Ok(match size {
            Size::Byte => u32::from(value),
            _ => (sign_extend(u32::from(value), Size::Byte) as u32) & mask_of(size),
        }),
        Operand::Imm16(value) => Ok(match size {
            Size::Byte => u32::from(value) & 0xff,
            Size::Word => u32::from(value),
            Size::Dword => u32::from(value),
        }),
        Operand::Imm32(value) => Ok(match size {
            Size::Byte => value & 0xff,
            Size::Word => value & 0xffff,
            Size::Dword => value,
        }),
        Operand::None => Err("instruction is missing an operand".into()),
    }
}

fn write_operand(
    memory: &mut GuestMemory,
    cpu: &mut Cpu,
    operand: Operand,
    size: Size,
    value: u32,
) -> Result<()> {
    match operand {
        Operand::Reg(register) => {
            cpu.regs.set(register, value);
            Ok(())
        }
        // A 16-bit register operand is 16 bits wide whatever the operation's
        // size says; the register itself is the destination type.
        Operand::Reg16(register) => {
            cpu.regs.set_word(register, value as u16);
            Ok(())
        }
        Operand::RegByte(register) => {
            cpu.regs.set_byte(register, value as u8);
            Ok(())
        }
        Operand::HighByte(register) => {
            cpu.regs.set_high_byte(register, value as u8);
            Ok(())
        }
        Operand::Mem(target) => {
            let address = address_of(cpu, target);
            match size {
                Size::Byte => memory.write_u8(address, value as u8),
                Size::Word => memory.write_u16(address, value as u16),
                Size::Dword => memory.write_u32(address, value),
            }
        }
        Operand::Imm8(_) | Operand::Imm16(_) | Operand::Imm32(_) => {
            Err("cannot write to an immediate operand".into())
        }
        Operand::None => Err("instruction is missing a destination".into()),
    }
}

fn push32(memory: &mut GuestMemory, cpu: &mut Cpu, value: u32) -> Result<()> {
    push_sized(memory, cpu, value, Size::Dword)
}

fn pop32(memory: &GuestMemory, cpu: &mut Cpu) -> Result<u32> {
    pop_sized(memory, cpu, Size::Dword)
}

/// Pushes at the operand width; `66 50` (`push ax`) moves two bytes, not four.
fn push_sized(memory: &mut GuestMemory, cpu: &mut Cpu, value: u32, size: Size) -> Result<()> {
    let width = size.bytes();
    let esp = cpu.regs.esp.wrapping_sub(width);
    cpu.regs.esp = esp;
    match size {
        Size::Byte => memory.write_u8(esp, value as u8),
        Size::Word => memory.write_u16(esp, value as u16),
        Size::Dword => memory.write_u32(esp, value),
    }
}

fn pop_sized(memory: &GuestMemory, cpu: &mut Cpu, size: Size) -> Result<u32> {
    let value = match size {
        Size::Byte => u32::from(memory.read_u8(cpu.regs.esp)?),
        Size::Word => u32::from(memory.read_u16(cpu.regs.esp)?),
        Size::Dword => memory.read_u32(cpu.regs.esp)?,
    };
    cpu.regs.esp = cpu.regs.esp.wrapping_add(size.bytes());
    Ok(value)
}

/// Computes `a op b` at `size` and records the flags the operation defines.
/// `Cmp` and `Test` share it and simply discard the result.
fn apply_alu(cpu: &mut Cpu, operation: Alu, a: u32, b: u32, size: Size) -> u32 {
    let mask = mask_of(size);
    let sign = sign_of(size);
    let a = a & mask;
    let b = b & mask;

    let carry_in = match operation {
        Alu::Adc | Alu::Sbb => u32::from(cpu.regs.flags.cf),
        _ => 0,
    };

    let (result, carry, overflow, adjust) = match operation {
        Alu::Add | Alu::Adc => {
            let wide = u64::from(a) + u64::from(b) + u64::from(carry_in);
            let result = (wide as u32) & mask;
            (
                result,
                wide > u64::from(mask),
                ((a ^ result) & (b ^ result) & sign) != 0,
                ((a ^ b ^ result) & 0x10) != 0,
            )
        }
        Alu::Sub | Alu::Sbb | Alu::Cmp => {
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
        Alu::Or => ((a | b) & mask, false, false, false),
        Alu::And | Alu::Test => ((a & b) & mask, false, false, false),
        Alu::Xor => ((a ^ b) & mask, false, false, false),
    };

    cpu.regs.flags.cf = carry;
    cpu.regs.flags.of = overflow;
    cpu.regs.flags.af = adjust;
    cpu.regs.flags.zf = result == 0;
    cpu.regs.flags.sf = result & sign != 0;
    cpu.regs.flags.pf = Flags::parity(result);
    result
}

fn condition_met(cpu: &Cpu, condition: Condition) -> bool {
    let flags = cpu.regs.flags;
    match condition {
        Condition::Overflow => flags.of,
        Condition::NotOverflow => !flags.of,
        Condition::Below => flags.cf,
        Condition::AboveOrEqual => !flags.cf,
        Condition::Equal => flags.zf,
        Condition::NotEqual => !flags.zf,
        Condition::BelowOrEqual => flags.cf || flags.zf,
        Condition::Above => !flags.cf && !flags.zf,
        Condition::Sign => flags.sf,
        Condition::NotSign => !flags.sf,
        Condition::Parity => flags.pf,
        Condition::NotParity => !flags.pf,
        Condition::Less => flags.sf != flags.of,
        Condition::GreaterOrEqual => flags.sf == flags.of,
        Condition::LessOrEqual => flags.zf || flags.sf != flags.of,
        Condition::Greater => !flags.zf && flags.sf == flags.of,
    }
}

/// One operand's value, used for the implicit-dual shifts and multiplies.
fn read_count(memory: &GuestMemory, cpu: &Cpu, operand: Operand) -> Result<u32> {
    let value = read_operand(memory, cpu, operand, Size::Byte)?;
    Ok(value & 0x1f)
}

fn shift_flags(cpu: &mut Cpu, result: u32, carry: bool, overflow: bool, size: Size) {
    cpu.regs.flags.cf = carry;
    cpu.regs.flags.of = overflow;
    cpu.regs.flags.zf = (result & mask_of(size)) == 0;
    cpu.regs.flags.sf = result & sign_of(size) != 0;
    cpu.regs.flags.pf = Flags::parity(result);
}

fn shift_left(value: u32, count: u32, size: Size) -> (u32, bool, bool) {
    let bits = bits_of(size);
    let mask = mask_of(size);
    let value = value & mask;
    if count >= bits {
        return (0, false, false);
    }
    let result = (value << count) & mask;
    let carry = (value >> (bits - count)) & 1 != 0;
    let overflow = if count == 1 {
        ((result & sign_of(size)) != 0) != carry
    } else {
        false
    };
    (result, carry, overflow)
}

fn shift_right(value: u32, count: u32, size: Size) -> (u32, bool, bool) {
    let bits = bits_of(size);
    let value = value & mask_of(size);
    if count >= bits {
        return (0, false, false);
    }
    let result = value >> count;
    let carry = (value >> (count - 1)) & 1 != 0;
    let overflow = if count == 1 {
        value & sign_of(size) != 0
    } else {
        false
    };
    (result, carry, overflow)
}

fn shift_arithmetic(value: u32, count: u32, size: Size) -> (u32, bool) {
    let bits = bits_of(size);
    let signed = sign_extend(value, size);
    let effective = count.min(bits - 1);
    let carry = (signed >> (effective.max(1) - 1)) & 1 != 0;
    let result = (signed >> effective) as u32 & mask_of(size);
    (result, carry)
}

fn rotate_left(value: u32, count: u32, size: Size) -> (u32, bool) {
    let bits = bits_of(size);
    let mask = mask_of(size);
    let effective = count % bits;
    let value = value & mask;
    if effective == 0 {
        return (value, value & sign_of(size) != 0);
    }
    let result = ((value << effective) | (value >> (bits - effective))) & mask;
    (result, result & 1 != 0)
}

fn rotate_right(value: u32, count: u32, size: Size) -> (u32, bool) {
    let bits = bits_of(size);
    let mask = mask_of(size);
    let effective = count % bits;
    let value = value & mask;
    if effective == 0 {
        return (value, value & sign_of(size) != 0);
    }
    let result = ((value >> effective) | (value << (bits - effective))) & mask;
    (result, result & sign_of(size) != 0)
}

fn rotate_left_through_carry(value: u32, count: u32, carry_in: bool, size: Size) -> (u32, bool) {
    let bits = bits_of(size);
    let mask = mask_of(size);
    let mut value = value & mask;
    let mut carry = carry_in;
    for _ in 0..(count % (bits + 1)) {
        let out = value & sign_of(size) != 0;
        value = ((value << 1) | u32::from(carry)) & mask;
        carry = out;
    }
    (value, carry)
}

fn rotate_right_through_carry(value: u32, count: u32, carry_in: bool, size: Size) -> (u32, bool) {
    let bits = bits_of(size);
    let mask = mask_of(size);
    let mut value = value & mask;
    let mut carry = carry_in;
    for _ in 0..(count % (bits + 1)) {
        let out = value & 1 != 0;
        value = (value >> 1) | (u32::from(carry) << (bits - 1));
        carry = out;
    }
    (value, carry)
}

fn multiply_unsigned(cpu: &mut Cpu, a: u32, b: u32, size: Size) {
    match size {
        Size::Byte => {
            let product = (a & 0xff) * (b & 0xff);
            let ax = (cpu.regs.get(EAX) & 0xffff_0000) | (product & 0xffff);
            cpu.regs.set(EAX, ax);
            let overflow = product > 0xff;
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
        }
        Size::Word => {
            let product = (a & 0xffff) * (b & 0xffff);
            cpu.regs.set_word(EAX, product as u16);
            cpu.regs.set_word(EDX, (product >> 16) as u16);
            let overflow = product > 0xffff;
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
        }
        Size::Dword => {
            let product = u64::from(a) * u64::from(b);
            cpu.regs.eax = product as u32;
            cpu.regs.edx = (product >> 32) as u32;
            let overflow = cpu.regs.edx != 0;
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
        }
    }
}

fn multiply_signed(cpu: &mut Cpu, a: u32, b: u32, size: Size) {
    match size {
        Size::Byte => {
            let product = i16::from(a as u8 as i8) * i16::from(b as u8 as i8);
            let ax = (cpu.regs.get(EAX) & 0xffff_0000) | u32::from(product as u16);
            cpu.regs.set(EAX, ax);
            let overflow = product != i16::from(product as i8);
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
        }
        Size::Word => {
            let product = i32::from(a as u16 as i16) * i32::from(b as u16 as i16);
            cpu.regs.set_word(EAX, product as u16);
            cpu.regs.set_word(EDX, (product >> 16) as u16);
            let overflow = product != i32::from(product as i16);
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
        }
        Size::Dword => {
            let product = i64::from(a as i32) * i64::from(b as i32);
            cpu.regs.eax = product as u32;
            cpu.regs.edx = (product >> 32) as u32;
            let overflow = product != i64::from(product as i32);
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
        }
    }
}

fn divide_unsigned(cpu: &mut Cpu, divisor: u32, size: Size) -> Result<()> {
    match size {
        Size::Byte => {
            let divisor = divisor & 0xff;
            if divisor == 0 {
                return Err("divide by zero".into());
            }
            let dividend = cpu.regs.get(EAX) & 0xffff;
            let quotient = dividend / divisor;
            if quotient > 0xff {
                return Err("divide overflow: the quotient does not fit in al".into());
            }
            let remainder = dividend % divisor;
            let ax = (cpu.regs.get(EAX) & 0xffff_0000) | ((remainder << 8) | quotient);
            cpu.regs.set(EAX, ax);
        }
        Size::Word => {
            let divisor = divisor & 0xffff;
            if divisor == 0 {
                return Err("divide by zero".into());
            }
            let dividend =
                (u32::from(cpu.regs.get_word(EDX)) << 16) | u32::from(cpu.regs.get_word(EAX));
            let quotient = dividend / divisor;
            if quotient > 0xffff {
                return Err("divide overflow: the quotient does not fit in ax".into());
            }
            cpu.regs.set_word(EAX, quotient as u16);
            cpu.regs.set_word(EDX, (dividend % divisor) as u16);
        }
        Size::Dword => {
            if divisor == 0 {
                return Err("divide by zero".into());
            }
            let dividend = (u64::from(cpu.regs.edx) << 32) | u64::from(cpu.regs.eax);
            let quotient = dividend / u64::from(divisor);
            if quotient > u64::from(u32::MAX) {
                return Err("divide overflow: the quotient does not fit in eax".into());
            }
            cpu.regs.eax = quotient as u32;
            cpu.regs.edx = (dividend % u64::from(divisor)) as u32;
        }
    }
    Ok(())
}

fn divide_signed(cpu: &mut Cpu, divisor: u32, size: Size) -> Result<()> {
    match size {
        Size::Byte => {
            let divisor = i32::from(divisor as u8 as i8);
            if divisor == 0 {
                return Err("divide by zero".into());
            }
            let dividend = i32::from((cpu.regs.get(EAX) & 0xffff) as u16 as i16);
            let quotient = dividend / divisor;
            if quotient > 0x7f || quotient < -0x80 {
                return Err("divide overflow: the quotient does not fit in al".into());
            }
            let remainder = dividend % divisor;
            let ax = (cpu.regs.get(EAX) & 0xffff_0000)
                | (((remainder as u32 & 0xff) << 8) | (quotient as u32 & 0xff));
            cpu.regs.set(EAX, ax);
        }
        Size::Word => {
            let divisor = i32::from(divisor as u16 as i16);
            if divisor == 0 {
                return Err("divide by zero".into());
            }
            let raw =
                (u32::from(cpu.regs.get_word(EDX)) << 16) | u32::from(cpu.regs.get_word(EAX));
            let dividend = i64::from(raw as i32);
            let quotient = dividend / i64::from(divisor);
            if quotient > 0x7fff || quotient < -0x8000 {
                return Err("divide overflow: the quotient does not fit in ax".into());
            }
            let remainder = dividend % i64::from(divisor);
            cpu.regs.set_word(EAX, quotient as u16);
            cpu.regs.set_word(EDX, remainder as u16);
        }
        Size::Dword => {
            let divisor = i64::from(divisor as i32);
            if divisor == 0 {
                return Err("divide by zero".into());
            }
            let dividend = ((u64::from(cpu.regs.edx) << 32) | u64::from(cpu.regs.eax)) as i64;
            if dividend == i64::MIN && divisor == -1 {
                return Err("divide overflow: the quotient does not fit in eax".into());
            }
            let quotient = dividend / divisor;
            if quotient > i64::from(i32::MAX) || quotient < i64::from(i32::MIN) {
                return Err("divide overflow: the quotient does not fit in eax".into());
            }
            let remainder = dividend % divisor;
            cpu.regs.eax = quotient as u32;
            cpu.regs.edx = remainder as u32;
        }
    }
    Ok(())
}

/// Reads one element at the string instruction's width.
fn read_element(memory: &GuestMemory, va: u32, size: Size) -> Result<u32> {
    match size {
        Size::Byte => Ok(u32::from(memory.read_u8(va)?)),
        Size::Word => Ok(u32::from(memory.read_u16(va)?)),
        Size::Dword => memory.read_u32(va),
    }
}

fn write_element(memory: &mut GuestMemory, va: u32, size: Size, value: u32) -> Result<()> {
    match size {
        Size::Byte => memory.write_u8(va, value as u8),
        Size::Word => memory.write_u16(va, value as u16),
        Size::Dword => memory.write_u32(va, value),
    }
}

/// Executes `Movs`/`Stos`/`Lods`/`Cmps`/`Scas`, including the `rep` forms.
///
/// The loop is written to touch memory directly rather than go through the
/// generic operand path: these are the CRT's `memcpy` and `memset`, and they run
/// once per copied byte.
fn string_operation(
    memory: &mut GuestMemory,
    cpu: &mut Cpu,
    instruction: &Instruction,
) -> Result<Step> {
    let size = instruction.size;
    let width = size.bytes();
    loop {
        if instruction.repeat != Repeat::None && cpu.regs.ecx == 0 {
            break;
        }
        let delta = if cpu.regs.flags.df {
            width.wrapping_neg()
        } else {
            width
        };
        match instruction.mnemonic {
            Mnemonic::Movs => {
                let value = read_element(memory, cpu.regs.esi, size)?;
                write_element(memory, cpu.regs.edi, size, value)?;
                cpu.regs.esi = cpu.regs.esi.wrapping_add(delta);
                cpu.regs.edi = cpu.regs.edi.wrapping_add(delta);
            }
            Mnemonic::Stos => {
                let value = cpu.regs.get(EAX);
                write_element(memory, cpu.regs.edi, size, value)?;
                cpu.regs.edi = cpu.regs.edi.wrapping_add(delta);
            }
            Mnemonic::Lods => {
                let value = read_element(memory, cpu.regs.esi, size)?;
                match size {
                    Size::Byte => cpu.regs.set_byte(EAX, value as u8),
                    Size::Word => cpu.regs.set_word(EAX, value as u16),
                    Size::Dword => cpu.regs.eax = value,
                }
                cpu.regs.esi = cpu.regs.esi.wrapping_add(delta);
            }
            Mnemonic::Cmps => {
                let left = read_element(memory, cpu.regs.esi, size)?;
                let right = read_element(memory, cpu.regs.edi, size)?;
                apply_alu(cpu, Alu::Cmp, left, right, size);
                cpu.regs.esi = cpu.regs.esi.wrapping_add(delta);
                cpu.regs.edi = cpu.regs.edi.wrapping_add(delta);
            }
            Mnemonic::Scas => {
                let left = cpu.regs.get(EAX) & mask_of(size);
                let right = read_element(memory, cpu.regs.edi, size)?;
                apply_alu(cpu, Alu::Cmp, left, right, size);
                cpu.regs.edi = cpu.regs.edi.wrapping_add(delta);
            }
            _ => return Err("string_operation called with a non-string instruction".into()),
        }
        if instruction.repeat == Repeat::None {
            break;
        }
        cpu.regs.ecx = cpu.regs.ecx.wrapping_sub(1);
        let repeats = match instruction.mnemonic {
            Mnemonic::Cmps | Mnemonic::Scas => match instruction.repeat {
                Repeat::WhileZero => cpu.regs.flags.zf,
                Repeat::WhileNotZero => !cpu.regs.flags.zf,
                Repeat::None => false,
            },
            _ => true,
        };
        if !repeats {
            break;
        }
    }
    Ok(Step::Continue)
}

// ---- x87 ----------------------------------------------------------------

fn read_float(
    memory: &GuestMemory,
    cpu: &Cpu,
    operand: Operand,
    double: bool,
    integer: bool,
    size: Size,
) -> Result<f64> {
    if integer {
        return read_integer(memory, cpu, operand, size, false);
    }
    let address = match operand {
        Operand::Mem(target) => address_of(cpu, target),
        _ => return Err("a floating-point load needs a memory operand".into()),
    };
    if double {
        let low = memory.read_u32(address)?;
        let high = memory.read_u32(address.wrapping_add(4))?;
        Ok(f64::from_bits(u64::from(low) | (u64::from(high) << 32)))
    } else {
        Ok(f64::from(f32::from_bits(memory.read_u32(address)?)))
    }
}

fn write_float(
    memory: &mut GuestMemory,
    cpu: &Cpu,
    operand: Operand,
    double: bool,
    value: f64,
) -> Result<()> {
    let address = match operand {
        Operand::Mem(target) => address_of(cpu, target),
        _ => return Err("a floating-point store needs a memory operand".into()),
    };
    if double {
        let bits = value.to_bits();
        memory.write_u32(address, bits as u32)?;
        memory.write_u32(address.wrapping_add(4), (bits >> 32) as u32)
    } else {
        memory.write_u32(address, (value as f32).to_bits())
    }
}

fn read_integer(
    memory: &GuestMemory,
    cpu: &Cpu,
    operand: Operand,
    size: Size,
    double: bool,
) -> Result<f64> {
    let address = match operand {
        Operand::Mem(target) => address_of(cpu, target),
        _ => return Err("an integer load into the FPU needs a memory operand".into()),
    };
    Ok(if double {
        let low = memory.read_u32(address)?;
        let high = memory.read_u32(address.wrapping_add(4))?;
        (u64::from(low) | (u64::from(high) << 32)) as i64 as f64
    } else if size == Size::Word {
        f64::from(memory.read_u16(address)? as i16)
    } else {
        f64::from(memory.read_u32(address)? as i32)
    })
}

fn write_integer(
    memory: &mut GuestMemory,
    cpu: &Cpu,
    operand: Operand,
    size: Size,
    double: bool,
    value: f64,
) -> Result<()> {
    let address = match operand {
        Operand::Mem(target) => address_of(cpu, target),
        _ => return Err("an integer store from the FPU needs a memory operand".into()),
    };
    // The hardware rounds toward zero here; Rust's float-to-int cast does too.
    let integer = value as i64;
    if double {
        memory.write_u32(address, integer as u32)?;
        memory.write_u32(address.wrapping_add(4), (integer as u64 >> 32) as u32)
    } else if size == Size::Word {
        memory.write_u16(address, integer as u16)
    } else {
        memory.write_u32(address, integer as u32)
    }
}

fn x87_operation(mnemonic: Mnemonic, left: f64, right: f64) -> f64 {
    match mnemonic {
        Mnemonic::Fadd | Mnemonic::Faddp => left + right,
        Mnemonic::Fmul | Mnemonic::Fmulp => left * right,
        Mnemonic::Fsub | Mnemonic::Fsubp => left - right,
        Mnemonic::Fsubr | Mnemonic::Fsubrp => right - left,
        Mnemonic::Fdiv | Mnemonic::Fdivp => left / right,
        Mnemonic::Fdivr | Mnemonic::Fdivrp => right / left,
        _ => f64::NAN,
    }
}

fn execute_x87(
    memory: &mut GuestMemory,
    cpu: &mut Cpu,
    instruction: &Instruction,
) -> Result<Step> {
    let size = instruction.size;
    match instruction.mnemonic {
        Mnemonic::Fld => {
            let value = match instruction.operand(0) {
                Operand::Imm8(index) => cpu.fpu.st(index),
                operand => read_float(memory, cpu, operand, instruction.double, instruction.integer, instruction.size)?,
            };
            cpu.fpu.push(value);
        }
        Mnemonic::Fst | Mnemonic::Fstp => {
            let value = cpu.fpu.st(0);
            write_float(memory, cpu, instruction.operand(0), instruction.double, value)?;
            if instruction.mnemonic == Mnemonic::Fstp {
                cpu.fpu.pop();
            }
        }
        Mnemonic::Fild => {
            let value = read_integer(memory, cpu, instruction.operand(0), size, instruction.double)?;
            cpu.fpu.push(value);
        }
        Mnemonic::Fist | Mnemonic::Fistp => {
            let value = cpu.fpu.st(0);
            write_integer(
                memory,
                cpu,
                instruction.operand(0),
                size,
                instruction.double,
                value,
            )?;
            if instruction.mnemonic == Mnemonic::Fistp {
                cpu.fpu.pop();
            }
        }
        Mnemonic::Fadd | Mnemonic::Fmul | Mnemonic::Fsub | Mnemonic::Fsubr | Mnemonic::Fdiv
        | Mnemonic::Fdivr => {
            let operand = instruction.operand(0);
            let destination = instruction.operand(1);
            let (left, right) = match (operand, destination) {
                (Operand::Imm8(source), Operand::None) => (cpu.fpu.st(0), cpu.fpu.st(source)),
                (Operand::None, Operand::Imm8(target)) => (cpu.fpu.st(target), cpu.fpu.st(0)),
                _ => (
                    cpu.fpu.st(0),
                    read_float(
                        memory,
                        cpu,
                        operand,
                        instruction.double,
                        instruction.integer,
                        instruction.size,
                    )?,
                ),
            };
            let result = x87_operation(instruction.mnemonic, left, right);
            match destination {
                Operand::Imm8(target) => cpu.fpu.set_st(target, result),
                _ => cpu.fpu.set_st(0, result),
            }
        }
        Mnemonic::Faddp | Mnemonic::Fmulp | Mnemonic::Fsubp | Mnemonic::Fsubrp
        | Mnemonic::Fdivp | Mnemonic::Fdivrp => {
            let target = match instruction.operand(1) {
                Operand::Imm8(index) => index,
                _ => 1,
            };
            let result = x87_operation(instruction.mnemonic, cpu.fpu.st(target), cpu.fpu.st(0));
            cpu.fpu.set_st(target, result);
            cpu.fpu.pop();
        }
        Mnemonic::Fcom | Mnemonic::Fcomp | Mnemonic::Fucom | Mnemonic::Fucomp => {
            let source = match instruction.operand(0) {
                Operand::Imm8(index) => cpu.fpu.st(index),
                operand => read_float(memory, cpu, operand, instruction.double, instruction.integer, instruction.size)?,
            };
            let top = cpu.fpu.st(0);
            cpu.fpu.compare_status(if top.is_nan() || source.is_nan() {
                None
            } else {
                top.partial_cmp(&source)
            });
            if matches!(
                instruction.mnemonic,
                Mnemonic::Fcomp | Mnemonic::Fucomp
            ) {
                cpu.fpu.pop();
            }
        }
        Mnemonic::Fcompp => {
            let top = cpu.fpu.st(0);
            let second = cpu.fpu.st(1);
            cpu.fpu.compare_status(if top.is_nan() || second.is_nan() {
                None
            } else {
                top.partial_cmp(&second)
            });
            cpu.fpu.pop();
            cpu.fpu.pop();
        }
        Mnemonic::Fldz => cpu.fpu.push(0.0),
        Mnemonic::Fld1 => cpu.fpu.push(1.0),
        Mnemonic::Fxch => {
            let index = match instruction.operand(0) {
                Operand::Imm8(index) => index,
                _ => 1,
            };
            let top = cpu.fpu.st(0);
            let other = cpu.fpu.st(index);
            cpu.fpu.set_st(0, other);
            cpu.fpu.set_st(index, top);
        }
        Mnemonic::Fchs => {
            let value = -cpu.fpu.st(0);
            cpu.fpu.set_st(0, value);
        }
        Mnemonic::Fabs => {
            let value = cpu.fpu.st(0).abs();
            cpu.fpu.set_st(0, value);
        }
        Mnemonic::Fsqrt => {
            let value = cpu.fpu.st(0).sqrt();
            cpu.fpu.set_st(0, value);
        }
        // `fsin`/`fcos` take the angle in radians and leave the result in place.
        // The guest's animation curves are built from these, so a wrong answer
        // here is a wrong shape on screen, not a rounding difference.
        Mnemonic::Fsin => {
            let value = cpu.fpu.st(0).sin();
            cpu.fpu.set_st(0, value);
        }
        Mnemonic::Fcos => {
            let value = cpu.fpu.st(0).cos();
            cpu.fpu.set_st(0, value);
        }
        Mnemonic::Fdecstp => cpu.fpu.rotate(-1),
        Mnemonic::Fincstp => cpu.fpu.rotate(1),
        Mnemonic::Fnstsw => {
            let status = u32::from(cpu.fpu.status);
            write_operand(memory, cpu, instruction.operand(0), Size::Word, status)?;
        }
        Mnemonic::Fnstcw => {
            let control = u32::from(cpu.fpu.control);
            write_operand(memory, cpu, instruction.operand(0), Size::Word, control)?;
        }
        Mnemonic::Fldcw => {
            let control = read_operand(memory, cpu, instruction.operand(0), Size::Word)?;
            cpu.fpu.control = control as u16;
        }
        Mnemonic::Fninit => cpu.fpu.reset(),
        Mnemonic::Fnclex => cpu.fpu.status &= !0x3f,
        other => return Err(format!("unimplemented x87 instruction {other:?}")),
    }
    Ok(Step::Continue)
}

/// Executes one decoded instruction.
pub fn execute(
    memory: &mut GuestMemory,
    cpu: &mut Cpu,
    instruction: &Instruction,
) -> Result<Step> {
    let size = instruction.size;
    if instruction.x87 {
        return execute_x87(memory, cpu, instruction);
    }
    match instruction.mnemonic {
        Mnemonic::Nop | Mnemonic::Wait => Ok(Step::Continue),
        Mnemonic::Hlt => Ok(Step::Halt),
        Mnemonic::Int3 => Err(format!(
            "guest executed int3 padding at {:#010x}",
            instruction.pc
        )),
        Mnemonic::Mov => {
            let value = read_operand(memory, cpu, instruction.operand(1), size)?;
            write_operand(memory, cpu, instruction.operand(0), size, value)?;
            Ok(Step::Continue)
        }
        Mnemonic::Movzx => {
            let value = read_operand(memory, cpu, instruction.operand(1), size)?;
            write_operand(memory, cpu, instruction.operand(0), Size::Dword, value)?;
            Ok(Step::Continue)
        }
        Mnemonic::Movsx => {
            let value = read_operand(memory, cpu, instruction.operand(1), size)?;
            let extended = sign_extend(value, size) as u32;
            write_operand(memory, cpu, instruction.operand(0), Size::Dword, extended)?;
            Ok(Step::Continue)
        }
        Mnemonic::Lea => {
            let Operand::Mem(target) = instruction.operand(1) else {
                return Err(format!(
                    "lea at {:#010x} has no memory operand",
                    instruction.pc
                ));
            };
            let address = address_of(cpu, target);
            write_operand(memory, cpu, instruction.operand(0), Size::Dword, address)?;
            Ok(Step::Continue)
        }
        Mnemonic::Xchg => {
            let left = read_operand(memory, cpu, instruction.operand(0), size)?;
            let right = read_operand(memory, cpu, instruction.operand(1), size)?;
            write_operand(memory, cpu, instruction.operand(0), size, right)?;
            write_operand(memory, cpu, instruction.operand(1), size, left)?;
            Ok(Step::Continue)
        }
        Mnemonic::Add
        | Mnemonic::Or
        | Mnemonic::Adc
        | Mnemonic::Sbb
        | Mnemonic::And
        | Mnemonic::Sub
        | Mnemonic::Xor
        | Mnemonic::Cmp
        | Mnemonic::Test => {
            let operation = match instruction.mnemonic {
                Mnemonic::Add => Alu::Add,
                Mnemonic::Or => Alu::Or,
                Mnemonic::Adc => Alu::Adc,
                Mnemonic::Sbb => Alu::Sbb,
                Mnemonic::And => Alu::And,
                Mnemonic::Sub => Alu::Sub,
                Mnemonic::Xor => Alu::Xor,
                Mnemonic::Cmp => Alu::Cmp,
                _ => Alu::Test,
            };
            let left = read_operand(memory, cpu, instruction.operand(0), size)?;
            let right = read_operand(memory, cpu, instruction.operand(1), size)?;
            let result = apply_alu(cpu, operation, left, right, size);
            if !matches!(operation, Alu::Cmp | Alu::Test) {
                write_operand(memory, cpu, instruction.operand(0), size, result)?;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Inc | Mnemonic::Dec => {
            let left = read_operand(memory, cpu, instruction.operand(0), size)?;
            let carry = cpu.regs.flags.cf;
            let operation = if instruction.mnemonic == Mnemonic::Inc {
                Alu::Add
            } else {
                Alu::Sub
            };
            let result = apply_alu(cpu, operation, left, 1, size);
            cpu.regs.flags.cf = carry;
            write_operand(memory, cpu, instruction.operand(0), size, result)?;
            Ok(Step::Continue)
        }
        Mnemonic::Neg => {
            let value = read_operand(memory, cpu, instruction.operand(0), size)?;
            let result = apply_alu(cpu, Alu::Sub, 0, value, size);
            write_operand(memory, cpu, instruction.operand(0), size, result)?;
            Ok(Step::Continue)
        }
        Mnemonic::Not => {
            let value = read_operand(memory, cpu, instruction.operand(0), size)?;
            write_operand(memory, cpu, instruction.operand(0), size, !value)?;
            Ok(Step::Continue)
        }
        Mnemonic::Mul => {
            let value = read_operand(memory, cpu, instruction.operand(0), size)?;
            let left = cpu.regs.get(EAX) & mask_of(size);
            multiply_unsigned(cpu, left, value, size);
            Ok(Step::Continue)
        }
        Mnemonic::Imul => {
            if instruction.operand(1) == Operand::None {
                let value = read_operand(memory, cpu, instruction.operand(0), size)?;
                let left = cpu.regs.get(EAX) & mask_of(size);
                multiply_signed(cpu, left, value, size);
                return Ok(Step::Continue);
            }
            let destination = read_operand(memory, cpu, instruction.operand(0), size)?;
            let source = read_operand(memory, cpu, instruction.operand(1), size)?;
            let factor = if instruction.operand(2) == Operand::None {
                destination
            } else {
                read_operand(memory, cpu, instruction.operand(2), size)?
            };
            let (result, overflow) = match size {
                Size::Word => {
                    let product = i32::from(source as u16 as i16) * i32::from(factor as u16 as i16);
                    (
                        (product as u32) & 0xffff,
                        product != i32::from(product as i16),
                    )
                }
                _ => {
                    let product = i64::from(source as i32) * i64::from(factor as i32);
                    (product as u32, product != i64::from(product as i32))
                }
            };
            cpu.regs.flags.cf = overflow;
            cpu.regs.flags.of = overflow;
            cpu.regs.flags.zf = result & mask_of(size) == 0;
            cpu.regs.flags.sf = result & sign_of(size) != 0;
            cpu.regs.flags.pf = Flags::parity(result);
            write_operand(memory, cpu, instruction.operand(0), size, result)?;
            Ok(Step::Continue)
        }
        Mnemonic::Div => {
            let divisor = read_operand(memory, cpu, instruction.operand(0), size)?;
            divide_unsigned(cpu, divisor, size)?;
            Ok(Step::Continue)
        }
        Mnemonic::Idiv => {
            let divisor = read_operand(memory, cpu, instruction.operand(0), size)?;
            divide_signed(cpu, divisor, size)?;
            Ok(Step::Continue)
        }
        Mnemonic::Shl | Mnemonic::Shr | Mnemonic::Sar | Mnemonic::Rol | Mnemonic::Ror
        | Mnemonic::Rcl | Mnemonic::Rcr => {
            let count = read_count(memory, cpu, instruction.operand(1))?;
            if count == 0 {
                return Ok(Step::Continue);
            }
            let value = read_operand(memory, cpu, instruction.operand(0), size)?;
            let (result, carry, overflow) = match instruction.mnemonic {
                Mnemonic::Shl => shift_left(value, count, size),
                Mnemonic::Shr => shift_right(value, count, size),
                Mnemonic::Sar => {
                    let (result, carry) = shift_arithmetic(value, count, size);
                    (result, carry, false)
                }
                Mnemonic::Rol => {
                    let (result, carry) = rotate_left(value, count, size);
                    let overflow = count == 1 && ((result & sign_of(size) != 0) != carry);
                    (result, carry, overflow)
                }
                Mnemonic::Ror => {
                    let (result, carry) = rotate_right(value, count, size);
                    let overflow = count == 1
                        && ((result & sign_of(size) != 0) != (result & (sign_of(size) >> 1) != 0));
                    (result, carry, overflow)
                }
                Mnemonic::Rcl => {
                    let (result, carry) =
                        rotate_left_through_carry(value, count, cpu.regs.flags.cf, size);
                    let overflow = count == 1 && ((result & sign_of(size) != 0) != carry);
                    (result, carry, overflow)
                }
                _ => {
                    let (result, carry) =
                        rotate_right_through_carry(value, count, cpu.regs.flags.cf, size);
                    let overflow = count == 1
                        && ((result & sign_of(size) != 0) != (result & (sign_of(size) >> 1) != 0));
                    (result, carry, overflow)
                }
            };
            shift_flags(cpu, result, carry, overflow, size);
            write_operand(memory, cpu, instruction.operand(0), size, result)?;
            Ok(Step::Continue)
        }
        Mnemonic::Shld | Mnemonic::Shrd => {
            let bits = bits_of(size);
            let mask = mask_of(size);
            let count = read_count(memory, cpu, instruction.operand(2))? % bits;
            if count == 0 {
                return Ok(Step::Continue);
            }
            let destination = read_operand(memory, cpu, instruction.operand(0), size)? & mask;
            let source = read_operand(memory, cpu, instruction.operand(1), size)? & mask;
            let (result, carry) = if instruction.mnemonic == Mnemonic::Shld {
                let result = ((destination << count) | (source >> (bits - count))) & mask;
                (result, (destination >> (bits - count)) & 1 != 0)
            } else {
                let result = ((destination >> count) | (source << (bits - count))) & mask;
                (result, (destination >> (count - 1)) & 1 != 0)
            };
            let overflow = count == 1 && ((result & sign_of(size) != 0) != carry);
            shift_flags(cpu, result, carry, overflow, size);
            write_operand(memory, cpu, instruction.operand(0), size, result)?;
            Ok(Step::Continue)
        }
        Mnemonic::Push => {
            let value = read_operand(memory, cpu, instruction.operand(0), size)?;
            push_sized(memory, cpu, value, size)?;
            Ok(Step::Continue)
        }
        Mnemonic::Pop => {
            let value = pop_sized(memory, cpu, size)?;
            write_operand(memory, cpu, instruction.operand(0), size, value)?;
            Ok(Step::Continue)
        }
        Mnemonic::Bt | Mnemonic::Bts | Mnemonic::Btr | Mnemonic::Btc => {
            let index = read_operand(memory, cpu, instruction.operand(1), size)?;
            // A memory operand is addressed by the bit index: the element holding
            // the bit is `index / bits` elements past the effective address.
            let bits = bits_of(size);
            let target = instruction.operand(0);
            let (operand, bit) = match target {
                Operand::Mem(m) => (
                    Operand::Mem(Mem {
                        disp: m
                            .disp
                            .wrapping_add((index / bits).wrapping_mul(size.bytes()) as i32),
                        ..m
                    }),
                    index % bits,
                ),
                other => (other, index % bits),
            };
            let value = read_operand(memory, cpu, operand, size)?;
            let mask = 1u32 << bit;
            let was_set = value & mask != 0;
            cpu.regs.flags.cf = was_set;
            let updated = match instruction.mnemonic {
                Mnemonic::Bts if !was_set => value | mask,
                Mnemonic::Btr if was_set => value & !mask,
                Mnemonic::Btc => value ^ mask,
                _ => return Ok(Step::Continue),
            };
            write_operand(memory, cpu, operand, size, updated)?;
            Ok(Step::Continue)
        }
        Mnemonic::Clc => {
            cpu.regs.flags.cf = false;
            Ok(Step::Continue)
        }
        Mnemonic::Stc => {
            cpu.regs.flags.cf = true;
            Ok(Step::Continue)
        }
        Mnemonic::Cmc => {
            cpu.regs.flags.cf = !cpu.regs.flags.cf;
            Ok(Step::Continue)
        }
        Mnemonic::Cli | Mnemonic::Sti => Ok(Step::Continue),
        Mnemonic::Cld => {
            cpu.regs.flags.df = false;
            Ok(Step::Continue)
        }
        Mnemonic::Std => {
            cpu.regs.flags.df = true;
            Ok(Step::Continue)
        }
        Mnemonic::PushSeg => {
            // A flat selector: nothing in this image uses a segment base.
            push32(memory, cpu, 0)?;
            Ok(Step::Continue)
        }
        Mnemonic::PopSeg => {
            let _ = pop32(memory, cpu)?;
            Ok(Step::Continue)
        }
        Mnemonic::Pushad => {
            let esp = cpu.regs.esp;
            for register in [
                cpu.regs.eax,
                cpu.regs.ecx,
                cpu.regs.edx,
                cpu.regs.ebx,
                esp,
                cpu.regs.ebp,
                cpu.regs.esi,
                cpu.regs.edi,
            ] {
                push32(memory, cpu, register)?;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Popad => {
            for index in [7u8, 6, 5] {
                let value = pop32(memory, cpu)?;
                cpu.regs.set(index, value);
            }
            // The saved ESP is discarded: the stack pointer already moved.
            let _ = pop32(memory, cpu)?;
            for index in [3u8, 2, 1, 0] {
                let value = pop32(memory, cpu)?;
                cpu.regs.set(index, value);
            }
            Ok(Step::Continue)
        }
        Mnemonic::Leave => {
            cpu.regs.esp = cpu.regs.ebp;
            cpu.regs.ebp = pop32(memory, cpu)?;
            Ok(Step::Continue)
        }
        Mnemonic::Pushfd => {
            push32(memory, cpu, cpu.regs.eflags())?;
            Ok(Step::Continue)
        }
        Mnemonic::Popfd => {
            let value = pop32(memory, cpu)?;
            cpu.regs.flags = Flags::from_eflags(value);
            Ok(Step::Continue)
        }
        Mnemonic::Lahf => {
            let flags = cpu.regs.flags;
            let ah = (u8::from(flags.sf) << 7)
                | (u8::from(flags.zf) << 6)
                | (u8::from(flags.af) << 4)
                | (u8::from(flags.pf) << 2)
                | 0x02
                | u8::from(flags.cf);
            cpu.regs.set_high_byte(EAX, ah);
            Ok(Step::Continue)
        }
        Mnemonic::Sahf => {
            let ah = cpu.regs.get_high_byte(EAX);
            let flags = &mut cpu.regs.flags;
            flags.cf = ah & 0x01 != 0;
            flags.pf = ah & 0x04 != 0;
            flags.af = ah & 0x10 != 0;
            flags.zf = ah & 0x40 != 0;
            flags.sf = ah & 0x80 != 0;
            Ok(Step::Continue)
        }
        Mnemonic::Jmp => {
            cpu.regs.eip = jump_target(memory, cpu, instruction.operand(0))?;
            Ok(Step::Continue)
        }
        Mnemonic::Loop | Mnemonic::Loope | Mnemonic::Loopne => {
            let count = cpu.regs.get(ECX).wrapping_sub(1);
            cpu.regs.set(ECX, count);
            let continue_loop = match instruction.mnemonic {
                Mnemonic::Loope => cpu.regs.flags.zf,
                Mnemonic::Loopne => !cpu.regs.flags.zf,
                _ => true,
            };
            if count != 0 && continue_loop {
                cpu.regs.eip = jump_target(memory, cpu, instruction.operand(0))?;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Jecxz => {
            if cpu.regs.get(ECX) == 0 {
                cpu.regs.eip = jump_target(memory, cpu, instruction.operand(0))?;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Jcc(condition) => {
            if condition_met(cpu, condition) {
                cpu.regs.eip = jump_target(memory, cpu, instruction.operand(0))?;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Call => {
            let return_address = cpu.regs.eip;
            let operand = instruction.operand(0);
            if let Operand::Mem(target) = operand {
                let address = address_of(cpu, target);
                if let Some(index) = cpu.table.index_of(address) {
                    let slot = cpu
                        .table
                        .slot(address)
                        .ok_or_else(|| format!("dispatch table is missing {address:#010x}"))?;
                    let arg_bytes = slot.arg_bytes();
                    let args = cpu.regs.esp;
                    let esp = args.wrapping_add(arg_bytes.unwrap_or(0));
                    cpu.regs.esp = esp;
                    return Ok(Step::HostCall(HostCall {
                        slot: index,
                        thunk: None,
                        iat_va: address,
                        return_address,
                        args,
                        esp,
                        arg_bytes,
                    }));
                }
                let target = memory.read_u32(address)?;
                push32(memory, cpu, return_address)?;
                cpu.regs.eip = target;
                return Ok(Step::Continue);
            }
            let target = jump_target(memory, cpu, operand)?;
            push32(memory, cpu, return_address)?;
            cpu.regs.eip = target;
            Ok(Step::Continue)
        }
        Mnemonic::Ret => {
            let target = pop32(memory, cpu)?;
            cpu.regs.eip = target;
            let released = match instruction.operand(0) {
                Operand::Imm8(value) => u32::from(value),
                Operand::Imm16(value) => u32::from(value),
                Operand::Imm32(value) => value,
                _ => 0,
            };
            cpu.regs.esp = cpu.regs.esp.wrapping_add(released);
            Ok(Step::Continue)
        }
        Mnemonic::Setcc(condition) => {
            let value = u32::from(condition_met(cpu, condition));
            write_operand(memory, cpu, instruction.operand(0), Size::Byte, value)?;
            Ok(Step::Continue)
        }
        Mnemonic::Cmovcc(condition) => {
            if condition_met(cpu, condition) {
                let value = read_operand(memory, cpu, instruction.operand(1), size)?;
                write_operand(memory, cpu, instruction.operand(0), size, value)?;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Xlat => {
            let index = cpu.regs.get_byte(EAX) as u32;
            let address = cpu.regs.ebx.wrapping_add(index);
            let value = memory.read_u8(address)?;
            cpu.regs.set_byte(EAX, value);
            Ok(Step::Continue)
        }
        Mnemonic::Cwde => {
            // `66 98` is `cbw`: sign-extend al into ax.
            if size == Size::Word {
                cpu.regs.set_word(EAX, cpu.regs.get_byte(EAX) as i8 as i16 as u16);
            } else {
                cpu.regs.eax = cpu.regs.eax as u16 as i16 as i32 as u32;
            }
            Ok(Step::Continue)
        }
        Mnemonic::Cdq => {
            if size == Size::Word {
                // `66 99` is `cwd`: sign-extend ax into dx.
                cpu.regs.set_word(EDX, if cpu.regs.get_word(EAX) & 0x8000 != 0 { 0xffff } else { 0 });
            } else {
                cpu.regs.edx = if cpu.regs.eax & 0x8000_0000 != 0 {
                    0xffff_ffff
                } else {
                    0
                };
            }
            Ok(Step::Continue)
        }
        Mnemonic::Movs | Mnemonic::Stos | Mnemonic::Lods | Mnemonic::Cmps | Mnemonic::Scas => {
            string_operation(memory, cpu, instruction)
        }
        // The x87 instructions are dispatched above; reaching here would mean a
        // new mnemonic was added without an implementation, which is a bug worth
        // naming rather than a silent no-op.
        other => Err(format!(
            "unimplemented instruction {other:?} at {:#010x}",
            instruction.pc
        )),
    }
}

fn jump_target(memory: &GuestMemory, cpu: &Cpu, operand: Operand) -> Result<u32> {
    match operand {
        Operand::Mem(target) => memory.read_u32(address_of(cpu, target)),
        other => read_operand(memory, cpu, other, Size::Dword),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{EAX, EBP, EBX, ECX, EDI, EDX, ESI};
    use crate::pe::IMAGE_BASE;

    fn machine() -> (GuestMemory, Cpu) {
        let memory = GuestMemory::empty(0x10000).expect("blank memory");
        let mut cpu = Cpu::at(IMAGE_BASE);
        cpu.regs.esp = IMAGE_BASE + 0x8000;
        (memory, cpu)
    }

    fn instruction(mnemonic: Mnemonic, size: Size, operands: [Operand; 3]) -> Instruction {
        Instruction::new(IMAGE_BASE + 0x100, 1, mnemonic, size, operands)
    }

    fn run(
        memory: &mut GuestMemory,
        cpu: &mut Cpu,
        instruction: &Instruction,
    ) -> Result<Step> {
        execute(memory, cpu, instruction)
    }

    #[test]
    fn add_sets_carry_overflow_and_sign() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0x7fff_ffff;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Add, Size::Dword, [Operand::Reg(EAX), Operand::Imm32(1), Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x8000_0000);
        assert!(cpu.regs.flags.of && cpu.regs.flags.sf);
        assert!(!cpu.regs.flags.cf && !cpu.regs.flags.zf);

        // 0x0f + 1 carries into the high nibble, which is AF.
        cpu.regs.eax = 0x0f;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Add, Size::Dword, [Operand::Reg(EAX), Operand::Imm32(1), Operand::None]),
        )
        .unwrap();
        assert!(cpu.regs.flags.af);
    }

    #[test]
    fn byte_add_carries_out_of_the_byte() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0xff;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Add, Size::Byte, [Operand::RegByte(EAX), Operand::Imm8(1), Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0);
        assert!(cpu.regs.flags.cf && cpu.regs.flags.zf && cpu.regs.flags.pf);
    }

    #[test]
    fn sub_and_cmp_borrow_without_writing() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Sub, Size::Dword, [Operand::Reg(EAX), Operand::Imm32(1), Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0xffff_ffff);
        assert!(cpu.regs.flags.cf && cpu.regs.flags.sf && !cpu.regs.flags.zf);

        cpu.regs.eax = 5;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Cmp, Size::Dword, [Operand::Reg(EAX), Operand::Imm32(5), Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 5, "cmp must not write");
        assert!(cpu.regs.flags.zf && !cpu.regs.flags.cf);
    }

    #[test]
    fn a_zero_shift_leaves_the_flags_alone() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0x1234;
        cpu.regs.flags.cf = true;
        cpu.regs.flags.zf = true;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Shl, Size::Dword, [Operand::Reg(EAX), Operand::Imm8(0), Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x1234);
        assert!(cpu.regs.flags.cf && cpu.regs.flags.zf);
    }

    #[test]
    fn shifts_move_the_top_bit_into_carry() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0x8000_0001;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Shl, Size::Dword, [Operand::Reg(EAX), Operand::Imm8(1), Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x0000_0002);
        assert!(cpu.regs.flags.cf);
        assert!(cpu.regs.flags.of, "sign changed, so OF is set for a count of one");
    }

    #[test]
    fn push_and_pop_round_trip_in_order() {
        let (mut memory, mut cpu) = machine();
        let start = cpu.regs.esp;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Push, Size::Dword, [Operand::Imm32(0x1122_3344), Operand::None, Operand::None]),
        )
        .unwrap();
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Push, Size::Dword, [Operand::Imm32(0x5566_7788), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.esp, start - 8);

        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Pop, Size::Dword, [Operand::Reg(EBX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.ebx, 0x5566_7788);
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Pop, Size::Dword, [Operand::Reg(EAX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x1122_3344);
        assert_eq!(cpu.regs.esp, start);
    }

    #[test]
    fn pushad_saves_the_stack_pointer_as_it_was() {
        let (mut memory, mut cpu) = machine();
        let start = cpu.regs.esp;
        cpu.regs.eax = 1;
        cpu.regs.ecx = 2;
        cpu.regs.edx = 3;
        cpu.regs.ebx = 4;
        cpu.regs.ebp = 5;
        cpu.regs.esi = 6;
        cpu.regs.edi = 7;

        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Pushad, Size::Dword, [Operand::None; 3]),
        )
        .unwrap();
        assert_eq!(cpu.regs.esp, start - 32);

        for register in [EAX, ECX, EDX, EBX, EBP, ESI, EDI] {
            cpu.regs.set(register, 0);
        }
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Popad, Size::Dword, [Operand::None; 3]),
        )
        .unwrap();
        assert_eq!(cpu.regs.esp, start);
        assert_eq!(
            [
                cpu.regs.eax,
                cpu.regs.ecx,
                cpu.regs.edx,
                cpu.regs.ebx,
                cpu.regs.ebp,
                cpu.regs.esi,
                cpu.regs.edi
            ],
            [1, 2, 3, 4, 5, 6, 7]
        );
    }

    #[test]
    fn a_conditional_jump_follows_its_flag() {
        let (mut memory, mut cpu) = machine();
        let after = cpu.regs.eip;
        cpu.regs.flags.zf = true;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Jcc(Condition::Equal), Size::Dword, [Operand::Imm32(IMAGE_BASE + 0x2000), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eip, IMAGE_BASE + 0x2000);

        cpu.regs.eip = after;
        cpu.regs.flags.zf = false;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Jcc(Condition::Equal), Size::Dword, [Operand::Imm32(IMAGE_BASE + 0x2000), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eip, after);
    }

    #[test]
    fn call_and_ret_round_trip_through_the_stack() {
        let (mut memory, mut cpu) = machine();
        let start = cpu.regs.esp;
        let return_address = cpu.regs.eip;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Call, Size::Dword, [Operand::Imm32(IMAGE_BASE + 0x400), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eip, IMAGE_BASE + 0x400);
        assert_eq!(cpu.regs.esp, start - 4);

        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Ret, Size::Dword, [Operand::None; 3]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eip, return_address);
        assert_eq!(cpu.regs.esp, start);
    }

    #[test]
    fn rep_stosd_fills_memory_and_clears_the_count() {
        let (mut memory, mut cpu) = machine();
        let destination = IMAGE_BASE + 0x2000;
        cpu.regs.edi = destination;
        cpu.regs.ecx = 4;
        cpu.regs.eax = 0xdead_beef;
        let mut stos = instruction(Mnemonic::Stos, Size::Dword, [Operand::None; 3]);
        stos.repeat = Repeat::WhileZero;

        run(&mut memory, &mut cpu, &stos).unwrap();
        assert_eq!(cpu.regs.ecx, 0);
        assert_eq!(cpu.regs.edi, destination + 16);
        assert_eq!(memory.read_u32(destination).unwrap(), 0xdead_beef);
        assert_eq!(memory.read_u32(destination + 12).unwrap(), 0xdead_beef);
    }

    #[test]
    fn rep_movsd_copies_bytes_backwards_when_the_direction_flag_is_set() {
        let (mut memory, mut cpu) = machine();
        let source = IMAGE_BASE + 0x3000;
        let destination = IMAGE_BASE + 0x4000;
        memory.write_bytes(source, b"abcdefgh").unwrap();
        cpu.regs.esi = source + 7;
        cpu.regs.edi = destination + 7;
        cpu.regs.ecx = 8;
        cpu.regs.flags.df = true;
        let mut movs = instruction(Mnemonic::Movs, Size::Byte, [Operand::None; 3]);
        movs.repeat = Repeat::WhileZero;

        run(&mut memory, &mut cpu, &movs).unwrap();
        for index in 0..8 {
            assert_eq!(
                memory.read_u8(destination + index).unwrap(),
                b"abcdefgh"[index as usize]
            );
        }
        assert_eq!(cpu.regs.ecx, 0);
        assert_eq!(cpu.regs.esi, source - 1);
        assert_eq!(cpu.regs.edi, destination - 1);
    }

    #[test]
    fn neg_of_the_most_negative_value_overflows() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0x8000_0000;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Neg, Size::Dword, [Operand::Reg(EAX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x8000_0000);
        assert!(cpu.regs.flags.of && cpu.regs.flags.cf && cpu.regs.flags.sf);
    }

    #[test]
    fn dividing_by_zero_is_an_error_not_a_value() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 10;
        let error = run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Div, Size::Dword, [Operand::Reg(EBX), Operand::None, Operand::None]),
        )
        .unwrap_err();
        assert!(error.contains("divide by zero"), "{error}");
    }

    #[test]
    fn fs_addressing_lands_in_the_teb() {
        let mut memory = GuestMemory::empty(0x0140_0000).expect("full window");
        let mut cpu = Cpu::at(IMAGE_BASE);
        memory.write_u32(TEB_BASE, 0x1234_5678).unwrap();
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Mov,
                Size::Dword,
                [
                    Operand::Reg(EAX),
                    Operand::Mem(Mem {
                        base: None,
                        index: None,
                        scale: 1,
                        disp: 0,
                        segment: Some(SEG_FS),
                    }),
                    Operand::None,
                ],
            ),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x1234_5678);
    }

    #[test]
    fn a_call_through_an_import_slot_asks_the_host() {
        let (mut memory, mut cpu) = machine();
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
        cpu.table = TABLE;
        let (iat_va, arg_bytes) = (SLOTS[0].iat_va, 20);
        let start = cpu.regs.esp;
        let return_address = cpu.regs.eip;
        let step = run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Call,
                Size::Dword,
                [Operand::Mem(Mem::direct(iat_va)), Operand::None, Operand::None],
            ),
        )
        .unwrap();

        let Step::HostCall(call) = step else {
            panic!("expected a host call, got {step:?}");
        };
        assert_eq!(call.iat_va, iat_va);
        assert_eq!(call.return_address, return_address);
        assert_eq!(call.arg_bytes, Some(arg_bytes));
        assert_eq!(call.esp, start + arg_bytes);
        assert_eq!(cpu.regs.esp, start + arg_bytes, "stdcall pops its arguments");
        assert_eq!(cpu.regs.eip, return_address, "the host returns to the call site");
    }

    #[test]
    fn word_arithmetic_carries_at_sixteen_bits() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0x1234_ffff;
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Add,
                Size::Word,
                [Operand::Reg16(EAX), Operand::Imm16(1), Operand::None],
            ),
        )
        .unwrap();
        // The carry comes out of bit 15, not bit 31, and the destination is a
        // 16-bit register, so the write zeroes the upper half as hardware does.
        assert_eq!(cpu.regs.eax, 0x0000_0000);
        assert!(cpu.regs.flags.cf && cpu.regs.flags.zf);
        assert!(!cpu.regs.flags.of && !cpu.regs.flags.sf);

        // 0x7fff + 1 overflows a signed word but not a signed dword.
        cpu.regs.eax = 0x7fff;
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Add,
                Size::Word,
                [Operand::Reg16(EAX), Operand::Imm16(1), Operand::None],
            ),
        )
        .unwrap();
        assert!(cpu.regs.flags.of && cpu.regs.flags.sf);
        assert!(!cpu.regs.flags.cf, "no unsigned carry out of 16 bits");
    }

    #[test]
    fn a_bit_test_answers_with_the_carry_and_writes_only_when_asked() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0x0000_0080;
        // `bt ax, 7`: the sign bit of the low byte, which is how this build tests
        // a pixel off a packed word without a mask and a compare.
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Bt,
                Size::Word,
                [Operand::Reg16(EAX), Operand::Imm8(7), Operand::None],
            ),
        )
        .unwrap();
        assert!(cpu.regs.flags.cf, "bit 7 of the word is set");
        assert_eq!(cpu.regs.eax, 0x0000_0080, "bt does not write its operand");

        // The bit index addresses past the operand: index 33 is bit 1 of the next
        // dword, which is what a memory bitmap test relies on.
        let base = IMAGE_BASE + 0x200;
        memory.write_u32(base, 0).unwrap();
        memory.write_u32(base + 4, 0).unwrap();
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Bts,
                Size::Dword,
                [
                    Operand::Mem(Mem::direct(base)),
                    Operand::Imm8(33),
                    Operand::None,
                ],
            ),
        )
        .unwrap();
        assert!(!cpu.regs.flags.cf, "the bit was clear before the test");
        assert_eq!(memory.read_u32(base).unwrap(), 0, "the first dword is untouched");
        assert_eq!(
            memory.read_u32(base + 4).unwrap(),
            2,
            "bts set bit 1 of the following dword"
        );
    }

    #[test]
    fn loop_counts_ecx_down_and_stops_at_zero() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.ecx = 2;
        cpu.regs.eip = IMAGE_BASE + 0x100;
        let jump = instruction(
            Mnemonic::Loop,
            Size::Dword,
            [Operand::Imm32(IMAGE_BASE + 0x40), Operand::None, Operand::None],
        );
        run(&mut memory, &mut cpu, &jump).unwrap();
        assert_eq!(cpu.regs.ecx, 1);
        assert_eq!(cpu.regs.eip, IMAGE_BASE + 0x40, "nonzero count jumps");

        cpu.regs.eip = IMAGE_BASE + 0x100;
        run(&mut memory, &mut cpu, &jump).unwrap();
        assert_eq!(cpu.regs.ecx, 0);
        assert_eq!(
            cpu.regs.eip,
            IMAGE_BASE + 0x100,
            "a zero count falls through"
        );
    }

    #[test]
    fn word_compare_sets_flags_from_sixteen_bits() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0xffff_ffff;
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Cmp,
                Size::Word,
                [Operand::Reg16(EAX), Operand::Imm16(0xffff), Operand::None],
            ),
        )
        .unwrap();
        assert!(cpu.regs.flags.zf, "0xffff == 0xffff at word width");
        assert!(cpu.regs.flags.cf == false);

        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Cmp,
                Size::Word,
                [Operand::Reg16(EAX), Operand::Imm16(0), Operand::None],
            ),
        )
        .unwrap();
        assert!(cpu.regs.flags.sf, "the sign comes from bit 15");
        assert!(!cpu.regs.flags.zf);
    }

    #[test]
    fn word_moves_go_through_memory_two_bytes_at_a_time() {
        let (mut memory, mut cpu) = machine();
        let address = IMAGE_BASE + 0x5000;
        memory.write_bytes(address, &[0xaa, 0xbb, 0xcc, 0xdd]).unwrap();

        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Mov,
                Size::Word,
                [
                    Operand::Reg16(EAX),
                    Operand::Mem(Mem::direct(address)),
                    Operand::None,
                ],
            ),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x0000_bbaa);

        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Mov,
                Size::Word,
                [
                    Operand::Mem(Mem::direct(address + 2)),
                    Operand::Reg16(EAX),
                    Operand::None,
                ],
            ),
        )
        .unwrap();
        assert_eq!(memory.read_bytes(address + 2, 2).unwrap(), &[0xaa, 0xbb]);
        assert_eq!(memory.read_u8(address + 4).unwrap(), 0, "only two bytes written");
    }

    #[test]
    fn a_word_push_moves_the_stack_pointer_by_two() {
        let (mut memory, mut cpu) = machine();
        let start = cpu.regs.esp;
        cpu.regs.eax = 0x1234_5678;

        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Push, Size::Word, [Operand::Reg16(EAX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.esp, start - 2);
        assert_eq!(memory.read_u16(start - 2).unwrap(), 0x5678);

        cpu.regs.eax = 0;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Pop, Size::Word, [Operand::Reg16(EAX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.esp, start);
        assert_eq!(cpu.regs.eax, 0x5678);
    }

    #[test]
    fn a_word_string_move_copies_two_bytes_per_element() {
        let (mut memory, mut cpu) = machine();
        let source = IMAGE_BASE + 0x6000;
        let destination = IMAGE_BASE + 0x6100;
        memory.write_bytes(source, b"abcdefgh").unwrap();
        cpu.regs.esi = source;
        cpu.regs.edi = destination;
        cpu.regs.ecx = 4;
        let mut movs = instruction(Mnemonic::Movs, Size::Word, [Operand::None; 3]);
        movs.repeat = Repeat::WhileZero;

        run(&mut memory, &mut cpu, &movs).unwrap();
        assert_eq!(memory.read_bytes(destination, 8).unwrap(), b"abcdefgh");
        assert_eq!(cpu.regs.esi, source + 8);
        assert_eq!(cpu.regs.edi, destination + 8);
        assert_eq!(cpu.regs.ecx, 0);
    }

    #[test]
    fn movsx_and_movzx_widen_a_word_source() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.ebx = 0x8001;
        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Movsx,
                Size::Word,
                [Operand::Reg(EAX), Operand::Reg16(EBX), Operand::None],
            ),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0xffff_8001);

        run(
            &mut memory,
            &mut cpu,
            &instruction(
                Mnemonic::Movzx,
                Size::Word,
                [Operand::Reg(EAX), Operand::Reg16(EBX), Operand::None],
            ),
        )
        .unwrap();
        assert_eq!(cpu.regs.eax, 0x0000_8001);
    }

    #[test]
    fn a_word_divide_uses_dx_ax() {
        let (mut memory, mut cpu) = machine();
        // dx:ax = 0x0001_86a0 = 100_000, so the quotient fits in ax.
        cpu.regs.eax = 0x0000_86a0;
        cpu.regs.edx = 0x0000_0001;
        cpu.regs.ebx = 7;

        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Div, Size::Word, [Operand::Reg16(EBX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.get_word(EAX), (100_000_u32 / 7) as u16);
        assert_eq!(cpu.regs.get_word(EDX), (100_000_u32 % 7) as u16);
        assert_eq!(cpu.regs.edx & 0xffff_0000, 0, "the rest of edx is untouched");
    }

    #[test]
    fn a_word_multiply_leaves_the_high_half_in_dx() {
        let (mut memory, mut cpu) = machine();
        cpu.regs.eax = 0xffff_ffff;
        cpu.regs.ebx = 0xffff;
        run(
            &mut memory,
            &mut cpu,
            &instruction(Mnemonic::Mul, Size::Word, [Operand::Reg16(EBX), Operand::None, Operand::None]),
        )
        .unwrap();
        assert_eq!(cpu.regs.get_word(EAX), 0x0001);
        assert_eq!(cpu.regs.get_word(EDX), 0xfffe);
        assert!(cpu.regs.flags.cf && cpu.regs.flags.of, "the product needs dx");
    }

    fn write_double(memory: &mut GuestMemory, address: u32, value: f64) {
        let bits = value.to_bits();
        memory.write_u32(address, bits as u32).unwrap();
        memory.write_u32(address + 4, (bits >> 32) as u32).unwrap();
    }

    fn read_double(memory: &GuestMemory, address: u32) -> f64 {
        let low = u64::from(memory.read_u32(address).unwrap());
        let high = u64::from(memory.read_u32(address + 4).unwrap());
        f64::from_bits(low | (high << 32))
    }

    fn x87_instruction(mnemonic: Mnemonic, operand: Operand, double: bool) -> Instruction {
        let mut instruction = instruction(mnemonic, Size::Dword, [operand, Operand::None, Operand::None]);
        instruction.double = double;
        instruction.x87 = true;
        instruction
    }

    #[test]
    fn floating_point_load_divide_and_store_round_trip() {
        let (mut memory, mut cpu) = machine();
        let left = IMAGE_BASE + 0x2000;
        let right = IMAGE_BASE + 0x2010;
        let out = IMAGE_BASE + 0x2020;
        write_double(&mut memory, left, 6.0);
        write_double(&mut memory, right, 4.0);

        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fld, Operand::Mem(Mem::direct(left)), true)).unwrap();
        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fld, Operand::Mem(Mem::direct(right)), true)).unwrap();
        assert_eq!(cpu.fpu.st(0), 4.0);
        assert_eq!(cpu.fpu.st(1), 6.0);

        // `fdiv st(1), st(0)`: the destination is named in operand 1.
        let mut divide = x87_instruction(Mnemonic::Fdiv, Operand::None, true);
        divide.operands[1] = Operand::Imm8(1);
        run(&mut memory, &mut cpu, &divide).unwrap();
        assert_eq!(cpu.fpu.st(1), 1.5);

        // The divide wrote st(1); exchange so the result can be stored.
        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fxch, Operand::Imm8(1), false)).unwrap();
        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fstp, Operand::Mem(Mem::direct(out)), true)).unwrap();
        assert_eq!(read_double(&memory, out), 1.5);
        assert_eq!(cpu.fpu.st(0), 4.0, "the exchanged value is what remains");
    }

    #[test]
    fn x87_arithmetic_reads_an_integer_memory_operand_as_an_integer() {
        let (mut memory, mut cpu) = machine();
        let value = IMAGE_BASE + 0x4000;
        memory.write_u32(value, 4).unwrap();
        cpu.fpu.push(3.0);

        let mut multiply = x87_instruction(Mnemonic::Fmul, Operand::Mem(Mem::direct(value)), false);
        multiply.integer = true;
        multiply.size = Size::Dword;
        run(&mut memory, &mut cpu, &multiply).unwrap();
        assert_eq!(cpu.fpu.st(0), 12.0, "fimul [4] against 3.0");

        // A 16-bit form reads the same way, and sign-extends.
        memory.write_u16(value, 0xffff).unwrap();
        let mut add = x87_instruction(Mnemonic::Fadd, Operand::Mem(Mem::direct(value)), false);
        add.integer = true;
        add.size = Size::Word;
        run(&mut memory, &mut cpu, &add).unwrap();
        assert_eq!(cpu.fpu.st(0), 11.0, "fiadd [-1] against 12.0");
    }

    #[test]
    fn the_x87_sine_and_cosine_work_on_the_top_of_the_stack() {
        let (mut memory, mut cpu) = machine();
        // sin(pi/2) is 1 and cos(pi) is -1: one quarter turn each, which is what
        // the guest's animation curves ask for.
        cpu.fpu.push(std::f64::consts::FRAC_PI_2);
        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fsin, Operand::None, false)).unwrap();
        assert!((cpu.fpu.st(0) - 1.0).abs() < 1e-12, "fsin left {}", cpu.fpu.st(0));

        cpu.fpu.pop();
        cpu.fpu.push(std::f64::consts::PI);
        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fcos, Operand::None, false)).unwrap();
        assert!((cpu.fpu.st(0) + 1.0).abs() < 1e-12, "fcos left {}", cpu.fpu.st(0));
    }

    #[test]
    fn a_floating_point_compare_sets_the_status_word_that_sahf_reads() {
        let (mut memory, mut cpu) = machine();
        let value = IMAGE_BASE + 0x3000;
        write_double(&mut memory, value, 2.0);

        run(&mut memory, &mut cpu, &x87_instruction(Mnemonic::Fld1, Operand::None, false)).unwrap();
        run(
            &mut memory,
            &mut cpu,
            &x87_instruction(Mnemonic::Fcomp, Operand::Mem(Mem::direct(value)), true),
        )
        .unwrap();
        // 1.0 < 2.0: C0 set, C2 and C3 clear, which lives in bits 8-10.
        assert_eq!(cpu.fpu.status & 0x4700, 0x0100);

        let mut status = x87_instruction(Mnemonic::Fnstsw, Operand::Reg(EAX), false);
        status.size = Size::Word;
        run(&mut memory, &mut cpu, &status).unwrap();
        assert_eq!(cpu.regs.get_word(EAX) & 0x4700, 0x0100);

        run(&mut memory, &mut cpu, &instruction(Mnemonic::Sahf, Size::Dword, [Operand::None; 3])).unwrap();
        assert!(cpu.regs.flags.cf, "C0 becomes CF, so `jbe` takes the branch");
        assert!(!cpu.regs.flags.zf);
    }
}
