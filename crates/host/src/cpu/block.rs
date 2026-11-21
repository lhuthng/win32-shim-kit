//! A cache of straight-line runs of decoded instructions.
//!
//! [`super::step`] answers one instruction at a time, and everything the caller
//! does around it -- the thread check, the trampoline compare, the native-
//! replacement lookup, the timer check, the tail record, the decode-cache probe
//! -- is paid once per instruction. Most of that work cannot change between two
//! instructions that are next to each other in the image: a run of instructions
//! with no branch in it cannot reach a host stub, cannot enter a native
//! replacement, and cannot give the CPU to another thread, because none of
//! those things can happen without a branch or a call.
//!
//! So a run is decoded once, kept, and executed as a unit. The caller pays its
//! per-instruction checks once per *run*, and this module pays the decode-cache
//! probe once per run instead of once per instruction. What is left per
//! instruction is the execute dispatch itself, which is the interpreter.
//!
//! A block is a straight line, never a graph: it ends after any instruction that
//! can change the program counter, so the next address is always either the next
//! instruction in the block or a fresh lookup. That is what makes the cache
//! correct without any bookkeeping about edges.
//!
//! `.text` is read-only in this host -- a write there is refused -- so a decoded
//! instruction cannot go stale, exactly as for [`super::Decodes`]. The image and
//! its import table are written before the first instruction runs, which is
//! before any block can be built.

use super::{execute, Cpu, Decodes, Instruction, Mnemonic, Step};
use crate::memory::GuestMemory;
use crate::target::HleEntry;
use crate::Result;

/// Entries in the block cache. Direct-mapped and indexed by address, like the
/// decode cache: a collision costs a rebuild and never a wrong answer.
const BLOCK_ENTRIES: usize = 1 << 12;

/// Instructions in one block.
///
/// Sixteen covers a loop body and a straight run of arithmetic, which is where
/// the win is, and stops a long stretch of code from holding one slot of the
/// cache for a run that is entered once.
const MAX_BLOCK: usize = 16;

/// Instructions the cache can hold before it is thrown away whole.
///
/// Blocks are kept in one growing pool rather than one allocation each, so a
/// block is a range in it and a rebuild costs nothing but the push. A block that
/// is evicted leaves its range behind, so the pool is emptied when it fills --
/// 32k instructions is about 1.5 MB, which is small next to the guest arena and
/// large enough that a real frame does not fill it.
const POOL_ENTRIES: usize = 1 << 15;

/// One cached run: where its instructions are in the pool, and how many.
#[derive(Clone, Copy, Default)]
struct Block {
    /// The address of the first instruction. A slot is empty when `len` is zero,
    /// so `pc` is only read for a block that has one.
    pc: u32,
    /// Index of the first instruction in [`Blocks::pool`].
    start: u32,
    len: u8,
}

/// The block cache.
pub struct Blocks {
    slots: Vec<Block>,
    pool: Vec<Instruction>,
    /// Addresses a block must not run into: the native replacements the target
    /// declared. Each of those is a routine the host runs instead of the guest
    /// code at that address, so a block that reached one would quietly execute
    /// the code the replacement exists to skip.
    barriers: &'static [HleEntry],
}

impl Blocks {
    pub fn new(barriers: &'static [HleEntry]) -> Self {
        Self {
            slots: vec![Block::default(); BLOCK_ENTRIES],
            pool: Vec::new(),
            barriers,
        }
    }

    /// Forgets every block. Only the pool running out calls this.
    fn clear(&mut self) {
        for slot in &mut self.slots {
            *slot = Block::default();
        }
        self.pool.clear();
    }

    /// Whether a native replacement answers for this address.
    ///
    /// A linear scan, and it is only reached while a block is being decoded, so
    /// it is paid once per block and not once per instruction. The table is a
    /// handful of entries or empty.
    #[inline]
    fn is_barrier(&self, at: u32) -> bool {
        self.barriers.iter().any(|entry| entry.address == at)
    }

    /// Decodes the run at `pc` into the pool and caches it.
    ///
    /// Answers `false` when there is nothing to cache -- the address is already
    /// a host stub, or a native replacement, or fails to decode at its first
    /// instruction -- and the caller then takes the one-instruction path, which
    /// is where those cases are handled.
    fn build(&mut self, memory: &GuestMemory, decodes: &mut Decodes, pc: u32) -> Result<bool> {
        if self.pool.len() + MAX_BLOCK > POOL_ENTRIES {
            self.clear();
        }
        let start = self.pool.len() as u32;
        let mut at = pc;
        let mut len = 0u8;
        while usize::from(len) < MAX_BLOCK {
            // A host stub is not guest code: its bytes are not an instruction and
            // running past it would run whatever happens to be next in the image.
            if at >= super::PROC_BASE || self.is_barrier(at) {
                break;
            }
            let instruction = match decodes.fetch(memory, at) {
                Ok(instruction) => *instruction,
                // A run that decodes up to a bad byte keeps what it has: the
                // instructions before it are valid and run first, and the bad one
                // faults when the program counter reaches it, exactly as the
                // one-instruction path would.
                Err(_) if len > 0 => break,
                Err(error) => return Err(error),
            };
            self.pool.push(instruction);
            len += 1;
            at = at.wrapping_add(u32::from(instruction.len));
            if ends_block(instruction.mnemonic) {
                break;
            }
        }
        if len == 0 {
            return Ok(false);
        }
        self.slots[slot_of(pc)] = Block { pc, start, len };
        Ok(true)
    }
}

/// The cache slot an address maps to.
///
/// Bit 0 is dropped because instructions are at least one byte and code is
/// two-byte aligned in practice, which keeps the two halves of a hot loop out of
/// one slot.
#[inline]
fn slot_of(pc: u32) -> usize {
    (pc as usize >> 1) & (BLOCK_ENTRIES - 1)
}

/// Whether an instruction can change the program counter, or end the run.
///
/// This is the whole correctness argument for the cache: everything the caller
/// checks once per run -- host stubs, native replacements, thread switches,
/// timers -- needs a branch or a call to reach, so a run that stops after every
/// one of those is a run in which none of them can happen in the middle.
#[inline]
fn ends_block(mnemonic: Mnemonic) -> bool {
    matches!(
        mnemonic,
        Mnemonic::Jmp
            | Mnemonic::Jcc(_)
            | Mnemonic::Call
            | Mnemonic::Ret
            | Mnemonic::Loop
            | Mnemonic::Loope
            | Mnemonic::Loopne
            | Mnemonic::Jecxz
            | Mnemonic::Hlt
            | Mnemonic::Int3
    )
}

/// Executes the run of instructions at the program counter.
///
/// This is [`super::step`] for a whole run: it answers the same [`Step`], having
/// retired up to [`MAX_BLOCK`] instructions, and leaves `cpu.instructions` and
/// `cpu.regs.eip` exactly as running them one at a time would.
pub fn step(
    memory: &mut GuestMemory,
    cpu: &mut Cpu,
    decodes: &mut Decodes,
    blocks: &mut Blocks,
) -> Result<Step> {
    let pc = cpu.regs.eip;
    // A host stub is answered by the one-instruction path, which is the only
    // place that knows about resolved names, import slots and object thunks.
    if pc >= super::PROC_BASE {
        return super::step(memory, cpu, decodes);
    }
    let slot = slot_of(pc);
    let cached = blocks.slots[slot];
    if cached.len == 0 || cached.pc != pc {
        if !blocks.build(memory, decodes, pc)? {
            return super::step(memory, cpu, decodes);
        }
    }
    let Block { start, len, .. } = blocks.slots[slot];
    // The range is checked once here rather than once per instruction: the loop
    // walks a slice, so the per-instruction bound check the index form would pay
    // is gone.
    let run = &blocks.pool[start as usize..start as usize + usize::from(len)];
    let mut ran = 0u32;
    let mut answer = Ok(Step::Continue);
    for instruction in run {
        // The return address a `call` pushes is the program counter after the
        // call, and the one-instruction path sets it before executing for the
        // same reason: the executor reads `eip` for it.
        cpu.regs.eip = instruction.pc.wrapping_add(u32::from(instruction.len));
        ran += 1;
        match execute(memory, cpu, instruction) {
            Ok(Step::Continue) => {}
            Ok(step) => {
                answer = Ok(step);
                break;
            }
            Err(error) => {
                // A fault leaves the program counter on the instruction that
                // faulted, not on the address after it: the report names what
                // went wrong, and inside a run the address after it is an
                // instruction that never executed.
                cpu.regs.eip = instruction.pc;
                answer = Err(error);
                break;
            }
        }
    }
    cpu.instructions += u64::from(ran);
    answer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::{Condition, Cpu, Decodes, Mnemonic, Operand, Size, Step, EAX, EBX};
    use crate::pe::IMAGE_BASE;

    const AT: u32 = IMAGE_BASE + 0x1000;

    /// `mov eax, 1; mov ebx, 2; add eax, ebx; nop; jmp +0; nop; hlt`
    ///
    /// Five instructions up to and including the jump, then a `nop` and a `hlt`
    /// that only the next run reaches.
    fn program() -> Vec<u8> {
        vec![
            0xb8, 0x01, 0x00, 0x00, 0x00, // mov eax, 1
            0xbb, 0x02, 0x00, 0x00, 0x00, // mov ebx, 2
            0x01, 0xd8, // add eax, ebx
            0x90, // nop
            0xeb, 0x00, // jmp +0 -- to the instruction after it
            0x90, // nop
            0xf4, // hlt
        ]
    }

    /// The program at [`AT`], with `nop` everywhere else.
    ///
    /// The padding matters: a run that walked off the end of the program would
    /// decode zero bytes, which are `add [eax], al`, and fault on a pointer that
    /// is not what the test is about.
    fn image(bytes: &[u8]) -> GuestMemory {
        let mut memory = GuestMemory::empty(0x2_0000).expect("blank memory");
        memory
            .write_bytes(IMAGE_BASE, &vec![0x90u8; 0x2_0000])
            .expect("the padding fits");
        memory.write_bytes(AT, bytes).expect("the program fits");
        memory
    }

    fn cpu_at(at: u32) -> Cpu {
        Cpu::at(at)
    }

    #[test]
    fn a_run_runs_every_instruction_in_it() {
        let mut memory = image(&program());
        let mut cpu = cpu_at(AT);
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&[]);

        assert_eq!(
            step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the run runs"),
            Step::Continue
        );
        assert_eq!(cpu.regs.get(EAX), 3, "mov, mov and add all ran");
        assert_eq!(cpu.regs.get(EBX), 2);
        // Four instructions, then the jump, which is the last one in the run.
        assert_eq!(cpu.instructions, 5);
        assert_eq!(cpu.regs.eip, AT + 15, "jmp +0 went to the instruction after it");
    }

    #[test]
    fn a_run_stops_after_a_branch_and_the_next_run_starts_there() {
        let mut memory = image(&program());
        let mut cpu = cpu_at(AT);
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&[]);

        step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the run runs");
        // The run ended on the jump, so the address after it is a new run: the
        // `nop` there, and the `hlt` that ends the next run in turn.
        let before = cpu.instructions;
        assert_eq!(
            step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the nop runs"),
            Step::Halt
        );
        assert_eq!(cpu.instructions - before, 2, "the nop and the hlt");
        assert_eq!(cpu.regs.eip, AT + 17);
    }

    #[test]
    fn the_second_visit_comes_from_the_cache() {
        let mut memory = image(&program());
        let mut cpu = cpu_at(AT);
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&[]);

        step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the run runs");
        let pooled = blocks.pool.len();
        cpu.regs.eip = AT;
        cpu.instructions = 0;
        step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the run runs again");
        assert_eq!(blocks.pool.len(), pooled, "nothing was decoded twice");
        assert_eq!(cpu.instructions, 5);
        assert_eq!(cpu.regs.get(EAX), 3);
    }

    #[test]
    fn a_barrier_stops_the_run_before_it() {
        // A native replacement answers for the second instruction, so the run
        // must not contain it: the host runs that address, not the guest.
        static BARRIER: [HleEntry; 1] = [HleEntry {
            address: AT + 5,
            kind: crate::target::Hle::Lzw,
        }];
        let mut memory = image(&program());
        let mut cpu = cpu_at(AT);
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&BARRIER);

        step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the run runs");
        assert_eq!(blocks.pool.len(), 1, "the run stopped before the barrier");
        assert_eq!(cpu.regs.eip, AT + 5);
        assert_eq!(cpu.regs.get(EBX), 0, "the second instruction did not run");
    }

    #[test]
    fn a_call_leaves_its_return_address_on_the_stack() {
        // `call +0` pushes the address after itself, and the executor reads it
        // from `eip`, so the run must set the program counter before executing.
        let mut memory = image(&[0xe8, 0x00, 0x00, 0x00, 0x00, 0x90]);
        let mut cpu = cpu_at(AT);
        let stack = IMAGE_BASE + 0x8000;
        cpu.regs.esp = stack;
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&[]);

        step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the call runs");
        assert_eq!(cpu.regs.eip, AT + 5);
        assert_eq!(
            memory.read_u32(stack - 4).expect("the return address"),
            AT + 5
        );
        assert_eq!(blocks.pool.len(), 1, "a call ends its run");
    }

    #[test]
    fn a_pool_that_fills_is_emptied_rather_than_grown() {
        let mut memory = image(&[0x90, 0x90, 0x90, 0x90, 0xeb, 0x00]);
        let mut cpu = cpu_at(AT);
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&[]);
        // Pretend a long session has filled the pool, which is what eventually
        // happens: evicted runs leave their instructions behind.
        blocks.pool = Vec::with_capacity(POOL_ENTRIES);
        for index in 0..POOL_ENTRIES - 1 {
            blocks.pool.push(Instruction::new(
                IMAGE_BASE + index as u32,
                1,
                Mnemonic::Nop,
                Size::Byte,
                [Operand::None; 3],
            ));
        }
        step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect("the run runs");
        assert!(
            blocks.pool.len() <= MAX_BLOCK,
            "the pool was emptied before the run was added"
        );
        assert_eq!(cpu.instructions, 5, "four nops and the jump");
    }

    #[test]
    fn a_fault_leaves_the_program_counter_on_the_instruction_that_faulted() {
        // `mov eax, 0; mov ecx, [eax]; nop` -- the load reads an address that
        // was never mapped, and the report has to name the load rather than the
        // `nop` after it, which never ran.
        let mut memory = image(&[0xb8, 0x00, 0x00, 0x00, 0x00, 0x8b, 0x08, 0x90]);
        let mut cpu = cpu_at(AT);
        let mut decodes = Decodes::new();
        let mut blocks = Blocks::new(&[]);

        let error = step(&mut memory, &mut cpu, &mut decodes, &mut blocks).expect_err("the load faults");
        assert!(error.contains("not mapped"), "{error}");
        assert_eq!(cpu.regs.eip, AT + 5, "the program counter names the load");
    }

    #[test]
    fn every_instruction_that_can_change_the_program_counter_ends_a_run() {
        // This is the cache's whole correctness argument, so it is asserted
        // rather than assumed: a run that did not stop after one of these would
        // execute the next instruction at an address the guest never asked for.
        for mnemonic in [
            Mnemonic::Jmp,
            Mnemonic::Jcc(Condition::Equal),
            Mnemonic::Call,
            Mnemonic::Ret,
            Mnemonic::Loop,
            Mnemonic::Loope,
            Mnemonic::Loopne,
            Mnemonic::Jecxz,
            Mnemonic::Hlt,
            Mnemonic::Int3,
        ] {
            assert!(ends_block(mnemonic), "{mnemonic:?} ends a run");
        }
        // And the ones that only read or write state must not, or a run would
        // never be longer than one instruction and the cache would buy nothing.
        for mnemonic in [
            Mnemonic::Mov,
            Mnemonic::Add,
            Mnemonic::Cmp,
            Mnemonic::Push,
            Mnemonic::Pop,
            Mnemonic::Nop,
            Mnemonic::Leave,
        ] {
            assert!(!ends_block(mnemonic), "{mnemonic:?} does not end a run");
        }
    }
}
