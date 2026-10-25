//! One running guest: the mapped image, the CPU, and a run loop that stops for
//! a reason instead of hanging.
//!
//! Everything the browser shell shows comes from here: the program counter, the
//! instruction count, and the outcome of the last run. A fault is an
//! [`Outcome::Fault`] value, not a panic, so the page can print it.

use crate::cpu::{self, Cpu, HostCall, Step};
use crate::dispatch::Table;
use crate::memory::GuestMemory;
use crate::pe::Image;
use crate::shim::{self, Action, GuestCall, Host};
use crate::target::{self, Target};
use crate::Result;

pub struct Session {
    /// What this machine is running, supplied by the port.
    pub target: &'static Target,
    pub memory: GuestMemory,
    pub cpu: Cpu,
    pub host: Host,
    /// Import calls the guest made, and how many of them a shim answered.
    pub host_calls: u64,
    pub host_returns: u64,
    /// The most recent import a shim answered, for tracing what the boot does.
    pub last_import: Option<String>,
    /// The register file of the frame a timer callback interrupted, kept until
    /// the callback returns through [`cpu::TIMER_TRAMPOLINE`]. Every register is
    /// part of it: the guest can hold a live value in `ecx` across the call the
    /// timer landed in, and a callback that clobbered it would corrupt the frame
    /// it interrupted.
    timer_frame: Option<(cpu::Registers, cpu::Fpu)>,
    /// Instruction count and timer id when the running timer callback was entered,
    /// so the callback's cost can be separated from the game's own work and
    /// attributed to the timer that ran it. `None` when no callback is running.
    timer_started: Option<(u64, u32)>,
    /// Total instructions retired inside timer callbacks so far. The BGM
    /// streamer decodes audio from a timer callback, and that decode is guest
    /// code competing with the game's per-frame budget.
    timer_instructions: u64,
    /// The same total split by timer id, which separates the audio refill from
    /// the game's own low-frequency timers.
    timer_by_id: [u64; 16],
    /// Host calls that are waiting for a guest routine: a window procedure a
    /// `DispatchMessageA` started, with the import's own frame suspended under
    /// it. A stack because a dispatched procedure can call another import that
    /// dispatches again; each [`cpu::CALL_TRAMPOLINE`] arrival finishes the top
    /// one with the routine's `eax`.
    call_frames: Vec<CallFrame>,
    /// The instruction count at which timers are looked at again. Timers only
    /// move when guest time does, so a run checks them every
    /// [`TIMER_CHECK_INSTRUCTIONS`] instead of on every instruction.
    timer_check_at: u64,
    /// State for the guest routines [`crate::hle`] runs natively, kept across
    /// calls the way the guest keeps its own.
    lzw: crate::hle::Lzw,
    blitter: crate::hle::Blitter,
    entry: u32,
    image_size: u32,
}

/// How often a run looks at the timers, in instructions. Guest time reaching a
/// chunk boundary is what makes a callback due; 256 instructions is a fraction of
/// a millisecond of guest time and keeps the check off the hot path.
const TIMER_CHECK_INSTRUCTIONS: u64 = 256;

/// A host call suspended while the guest runs a routine for it.
struct CallFrame {
    /// Everything but `eax`, which the routine's return value replaces.
    regs: cpu::Registers,
    fpu: cpu::Fpu,
    /// Where the import's own arguments start, so the call can be popped when the
    /// routine returns.
    args: u32,
    arg_bytes: u32,
}

/// What [`Session::serve`] did with a host call.
enum Served {
    /// The guest continues with this value in `eax`.
    Return(u32),
    /// The guest asked to terminate.
    Exit(u32),
    /// The guest is now running a routine the shim asked for; `eax` is the
    /// routine's to fill.
    Call,
}

/// Why a run stopped. `Steps` means the budget ran out with the guest still
/// running; the next call continues where this one stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Steps,
    Halted,
    HostCall(HostCall),
    Fault(String),
}

impl Outcome {
    /// Stable code for the wasm boundary: 0 steps, 1 halted, 2 host call,
    /// 3 fault.
    pub fn code(&self) -> i32 {
        match self {
            Outcome::Steps => 0,
            Outcome::Halted => 1,
            Outcome::HostCall(_) => 2,
            Outcome::Fault(_) => 3,
        }
    }

    /// One sentence for the log, naming the slot when the target's table has
    /// one. The table is passed in because it belongs to the target, not here.
    pub fn describe(&self, table: Table) -> String {
        match self {
            Outcome::Steps => "instruction budget exhausted".to_string(),
            Outcome::Halted => "halted".to_string(),
            Outcome::HostCall(call) => format!(
                "host call {} at {:#010x}, returns to {:#010x}, esp {:#010x}",
                label(call, table),
                call.iat_va,
                call.return_address,
                call.esp
            ),
            Outcome::Fault(message) => message.clone(),
        }
    }
}

impl Session {
    /// Parses the executable and maps it at its fixed base, for a port that has
    /// no target of its own (the tests in this crate).
    pub fn load(bytes: Vec<u8>) -> Result<Self> {
        Self::load_for(bytes, &target::DEFAULT)
    }

    /// Parses the executable and maps it at its fixed base, under `target`'s
    /// import surface and shims.
    pub fn load_for(bytes: Vec<u8>, target: &'static Target) -> Result<Self> {
        let image = Image::parse(bytes)?;
        let entry = image.entry_point;
        let image_size = image.size_of_image;
        let mut memory = GuestMemory::load(image)?;
        // Fill the import table with host stubs, which is what a real loader
        // does with DLL addresses. Without this, code that loads a slot into a
        // register and calls through it would jump at the loader's leftover
        // hint/name RVA.
        for (index, slot) in target.table.iter().enumerate() {
            memory.write_import(slot.iat_va, cpu::HOST_FUNCTION_BASE + index as u32 * 4)?;
        }
        // The loader enters `WinMainCRTStartup` with a stack, exactly as
        // Windows does; nothing is called, so there is no return address.
        let mut cpu = Cpu::with_table(entry, target.table);
        cpu.regs.esp = crate::memory::STACK_TOP;
        cpu.regs.ebp = 0;
        let mut host = Host::with_target(target);
        // A device the browser cannot have is supplied as a name and a length:
        // the guest measures the file and never reads it. The names come from
        // the target, so no product's disc layout lives in this crate.
        for (name, size) in target.provided_files {
            host.files.provide_sized(name, *size);
            host.notice(format!(
                "supplied marker: {name} ({size} bytes) stands in for a device the browser cannot have"
            ));
        }
        Ok(Self {
            target,
            memory,
            cpu,
            host,
            host_calls: 0,
            host_returns: 0,
            last_import: None,
            timer_frame: None,
            timer_started: None,
            timer_instructions: 0,
            timer_by_id: [0; 16],
            call_frames: Vec::new(),
            timer_check_at: 0,
            lzw: crate::hle::Lzw::new(),
            blitter: crate::hle::Blitter::new(),
            entry,
            image_size,
        })
    }

    pub fn entry(&self) -> u32 {
        self.entry
    }

    pub fn image_size(&self) -> u32 {
        self.image_size
    }

    pub fn pc(&self) -> u32 {
        self.cpu.regs.eip
    }

    pub fn instructions(&self) -> u64 {
        self.cpu.instructions
    }

    /// Sprite blits the host drew instead of the interpreter, and the pixels
    /// they covered, so a moving frame can be attributed without a sample.
    pub fn blit_calls(&self) -> u64 {
        self.blitter.calls()
    }

    pub fn blit_pixels(&self) -> u64 {
        self.blitter.pixels()
    }

    /// Instructions retired inside timer callbacks. Subtract this from
    /// [`Self::instructions`] to get what the game itself ran.
    pub fn timer_instructions(&self) -> u64 {
        self.timer_instructions
    }

    /// Instructions retired inside the callbacks of one timer id.
    pub fn timer_instructions_for(&self, id: u32) -> u64 {
        self.timer_by_id.get(id as usize).copied().unwrap_or(0)
    }

    /// Sets the guest's clock in milliseconds. Whoever drives the session supplies
    /// it -- the page before every chunk, from its own `performance.now()` -- so
    /// the guest's timers keep real-time intervals instead of tracking how many
    /// instructions this interpreter retires per second.
    pub fn set_time_ms(&mut self, milliseconds: u64) {
        self.host.set_clock(milliseconds);
    }

    /// Executes up to `budget` instructions, stopping early for a halt, a host
    /// call or a fault.
    pub fn run(&mut self, budget: u32) -> Outcome {
        for _ in 0..budget {
            // Both trampolines live above the host-call base, and no guest code
            // does, so one compare keeps the two checks out of the common path.
            let eip = self.cpu.regs.eip;
            if eip >= cpu::HOST_FUNCTION_BASE {
                // A timer callback is a guest routine the host calls on an
                // interval. Its return comes back through the trampoline, which
                // is not code the guest can reach, so the frame it interrupted is
                // restored here rather than by anything on the stack.
                if eip == cpu::CALL_TRAMPOLINE {
                    let Some(frame) = self.call_frames.pop() else {
                        return Outcome::Fault(format!(
                            "returned to the call trampoline {:#010x} with no host call waiting",
                            cpu::CALL_TRAMPOLINE
                        ));
                    };
                    // The routine's result is the import's return value, and the
                    // import's frame -- arguments still on the stack -- is restored.
                    let result = self.cpu.regs.eax;
                    self.cpu.regs = frame.regs;
                    self.cpu.fpu = frame.fpu;
                    self.cpu.regs.eax = result;
                    self.cpu.regs.esp = frame.args.wrapping_add(frame.arg_bytes);
                    continue;
                }
                if eip == cpu::TIMER_TRAMPOLINE {
                    if let Some((started, id)) = self.timer_started.take() {
                        let cost = self.cpu.instructions - started;
                        self.timer_instructions += cost;
                        if let Some(slot) = self.timer_by_id.get_mut(id as usize) {
                            *slot += cost;
                        }
                    }
                    match self.timer_frame.take() {
                        Some((regs, fpu)) => {
                            self.cpu.regs = regs;
                            self.cpu.fpu = fpu;
                        }
                        None => {
                            return Outcome::Fault(format!(
                                "returned to the timer trampoline {:#010x} with no callback running",
                                cpu::TIMER_TRAMPOLINE
                            ))
                        }
                    }
                    continue;
                }
            }
            // The image decoder is pure computation on guest memory, and it is
            // the largest single consumer of guest instructions on the board, so
            // the host runs it natively. It keeps its own stack contract; only
            // the instruction budget it stands for is added here.
            match eip {
                crate::hle::LZW_DECODE => {
                    match self.lzw.decode(&mut self.memory, &mut self.cpu) {
                        Ok(work) => self.cpu.instructions += u64::from(work),
                        Err(error) => return Outcome::Fault(error),
                    }
                    continue;
                }
                // The sprite blitters redraw the board on every moving frame.
                // Their keyed inner loop is ten guest instructions per pixel,
                // so the host decodes the row stream and writes the pixels
                // itself, keeping the guest's stack and scratch contract.
                crate::hle::BLIT_OPAQUE | crate::hle::BLIT_KEYED => {
                    let keyed = eip == crate::hle::BLIT_KEYED;
                    match self.blitter.blit(&mut self.memory, &mut self.cpu, keyed) {
                        Ok(work) => self.cpu.instructions += u64::from(work),
                        Err(error) => return Outcome::Fault(error),
                    }
                    continue;
                }
                _ => {}
            }
            // Timers change only as guest time moves, which is at most once per
            // chunk when the embedder supplies a clock, so looking one instruction
            // at a time buys nothing and costs a call, a scan and a division on
            // every instruction of every run.
            if self.timer_frame.is_none() && self.cpu.instructions >= self.timer_check_at {
                self.timer_check_at = self.cpu.instructions + TIMER_CHECK_INSTRUCTIONS;
                if let Some(fault) = self.fire_timer() {
                    return Outcome::Fault(fault);
                }
            }
            match cpu::step(&mut self.memory, &mut self.cpu) {
                Ok(Step::Continue) => {}
                Ok(Step::Halt) => return Outcome::Halted,
                Ok(Step::HostCall(call)) => {
                    self.host_calls += 1;
                    match self.serve(&call) {
                        Ok(Some(Served::Return(value))) => {
                            self.cpu.regs.eax = value;
                            self.host_returns += 1;
                        }
                        Ok(Some(Served::Exit(code))) => {
                            self.host.exit_code = Some(code);
                            return Outcome::Halted;
                        }
                        // The shim asked the guest to run a routine first; the
                        // program counter is already on it.
                        Ok(Some(Served::Call)) => {}
                        // No shim yet: this is the diagnostic, not a failure.
                        Ok(None) => return Outcome::HostCall(call),
                        Err(error) => return Outcome::Fault(error),
                    }
                }
                Err(error) => return Outcome::Fault(error),
            }
        }
        Outcome::Steps
    }

    /// Starts a due timer callback, if one is waiting and none is running.
    ///
    /// The callback is entered the way the timer thread would enter it: the five
    /// stdcall arguments are pushed (right to left) and a return address above
    /// them, so the routine's own `ret 20` unwinds exactly as it does under
    /// Windows. The interrupted `eip`/`esp` wait in [`Session::timer_frame`].
    fn fire_timer(&mut self) -> Option<String> {
        if self.timer_frame.is_some() {
            return None;
        }
        let now = self.host.guest_time_ms(self.cpu.instructions);
        let (id, callback, user) = self.host.timers.due(now)?;
        let mut esp = self.cpu.regs.esp;
        for value in [0, 0, user, 0, id] {
            esp = esp.wrapping_sub(4);
            if let Err(error) = self.memory.write_u32(esp, value) {
                return Some(format!("timer #{id} could not push its arguments: {error}"));
            }
        }
        esp = esp.wrapping_sub(4);
        if let Err(error) = self.memory.write_u32(esp, cpu::TIMER_TRAMPOLINE) {
            return Some(format!("timer #{id} could not push its return address: {error}"));
        }
        self.timer_frame = Some((self.cpu.regs, self.cpu.fpu.clone()));
        self.timer_started = Some((self.cpu.instructions, id));
        self.cpu.regs.esp = esp;
        self.cpu.regs.eip = callback;
        None
    }

    /// Runs the shim for a call, or reports that there is none. On success the
    /// stack is popped by the shim's own declared signature, which is what a
    /// stdcall callee does; the dispatch table's arity is a cross-check, never
    /// the source of truth.
    fn serve(&mut self, call: &HostCall) -> Result<Option<Served>> {
        // A call through a host object carries its own interface and method, and
        // the argument count comes from the same table the step loop used.
        if let Some((kind, method)) = call.thunk {
            let arg_bytes = crate::thunks::arg_bytes(kind, method);
            let mut arguments = shim::Call {
                memory: &mut self.memory,
                args: call.args,
                arg_bytes,
            };
            let action = crate::thunks::serve(&mut self.host, &mut arguments, kind, method)?;
            self.last_import = Some(format!(
                "{} args={:#010x} eax={:#010x}",
                crate::thunks::label(kind, method),
                call.args,
                match action {
                    Action::Return(value) => value,
                    Action::Exit(code) => code,
                }
            ));
            return self.finish(action, call.args, arg_bytes).map(Some);
        }
        let Some(slot) = self.target.table.slots.get(call.slot) else {
            return Err(format!("slot {} is outside the dispatch table", call.slot));
        };
        let Some(binding) = shim::binding(self.target.shims, slot) else {
            return Ok(None);
        };
        if let Some(agreed) = slot.arg_bytes() {
            if agreed != binding.arg_bytes {
                return Err(format!(
                    "{}: the shim declares {} argument bytes, but the call sites agree on {}",
                    slot.label(),
                    binding.arg_bytes,
                    agreed
                ));
            }
        }

        let mut arguments = shim::Call {
            memory: &mut self.memory,
            args: call.args,
            arg_bytes: binding.arg_bytes,
        };
        let action = (binding.shim)(&mut self.host, &mut arguments)?;
        self.last_import = Some(format!(
            "{} args={:#010x} eax={:#010x}",
            slot.label(),
            call.args,
            match action {
                Action::Return(value) => value,
                Action::Exit(code) => code,
            }
        ));
        self.finish(action, call.args, binding.arg_bytes).map(Some)
    }

    /// Completes a host call whose shim has run.
    ///
    /// A shim that needs guest code leaves a [`GuestCall`] in the host instead of
    /// returning a value: the routine is entered here with its arguments pushed
    /// and `CALL_TRAMPOLINE` as its return address, the import's own frame is
    /// suspended under it, and the value the routine leaves in `eax` finishes the
    /// import when the trampoline is reached.
    fn finish(&mut self, action: Action, args: u32, arg_bytes: u32) -> Result<Served> {
        if let Some(request) = self.host.guest_call.take() {
            self.start_guest_call(request, args, arg_bytes)?;
            return Ok(Served::Call);
        }
        // stdcall: the callee pops its arguments.
        self.cpu.regs.esp = args.wrapping_add(arg_bytes);
        Ok(match action {
            Action::Return(value) => Served::Return(value),
            Action::Exit(code) => Served::Exit(code),
        })
    }

    /// Pushes a guest routine's arguments and enters it, saving the frame the
    /// import was called from.
    ///
    /// A shim asks for this by leaving a [`GuestCall`] in the host rather than by
    /// calling it; it is public so a port can drive a routine directly when it
    /// needs to probe the guest.
    pub fn start_guest_call(&mut self, request: GuestCall, args: u32, arg_bytes: u32) -> Result<()> {
        let mut esp = self.cpu.regs.esp;
        for value in request.args.iter().rev() {
            esp = esp.wrapping_sub(4);
            self.memory.write_u32(esp, *value)?;
        }
        esp = esp.wrapping_sub(4);
        self.memory.write_u32(esp, cpu::CALL_TRAMPOLINE)?;
        self.call_frames.push(CallFrame {
            regs: self.cpu.regs,
            fpu: self.cpu.fpu.clone(),
            args,
            arg_bytes,
        });
        self.cpu.regs.esp = esp;
        self.cpu.regs.eip = request.entry;
        Ok(())
    }

    /// Whether a guest routine a shim asked for is still running.
    pub fn in_guest_call(&self) -> bool {
        !self.call_frames.is_empty()
    }

    pub fn exit_code(&self) -> Option<u32> {
        self.host.exit_code
    }

    /// Whether a timer callback is running, which the page shows as part of the
    /// guest's state rather than as a stop.
    pub fn in_timer_callback(&self) -> bool {
        self.timer_frame.is_some()
    }

    /// `DLL!Function` for the slot a host call went through.
    pub fn label(&self, call: &HostCall) -> String {
        label(call, self.target.table)
    }
}

fn label(call: &HostCall, table: Table) -> String {
    if let Some((kind, method)) = call.thunk {
        return crate::thunks::label(kind, method);
    }
    table
        .slot(call.iat_va)
        .map(|slot| slot.label())
        .unwrap_or_else(|| format!("slot {} ({:#010x})", call.slot, call.iat_va))
}

