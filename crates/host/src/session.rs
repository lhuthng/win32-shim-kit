//! One running guest: the mapped image, the CPU, and a run loop that stops for
//! a reason instead of hanging.
//!
//! Everything the browser shell shows comes from here: the program counter, the
//! instruction count, and the outcome of the last run. A fault is an
//! [`Outcome::Fault`] value, not a panic, so the page can print it.

use crate::cpu::{self, Cpu, HostCall, Step};
use crate::dispatch::Table;
use crate::memory::GuestMemory;
use crate::objects::WAIT_OBJECT_0;
use crate::pe::Image;
use crate::sched::{CallFrame, Spawn, ThreadSummary, ThreadWant, Threads};
use crate::shim::{self, Action, GuestCall, Host};
use crate::target::{self, Target};
use crate::Result;

pub struct Session {
    /// What this machine is running, supplied by the port.
    pub target: &'static Target,
    /// The shim for each slot of `target.table`, resolved once at load.
    ///
    /// A host call is the most frequent thing a guest does, and resolving it by
    /// scanning every shim table each time was a string search per candidate per
    /// call -- in the browser profile it was ninety percent of the page's time.
    /// The answer cannot change while the session lives, so it is worked out here
    /// and indexed by slot.
    bindings: Vec<Option<&'static shim::Binding>>,
    pub memory: GuestMemory,
    pub cpu: Cpu,
    /// The decoded-instruction cache, which lives beside the CPU so the step
    /// loop can read an instruction it has already decoded without copying it.
    pub(crate) decodes: cpu::Decodes,
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
    ///
    /// This is the *running* thread's frame. When the CPU changes threads it
    /// moves into that thread's record and the next thread's moves here, so
    /// everything below reads exactly as it did when this host had one thread.
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
    /// one with the routine's `eax`. Like the timer frame, this is the running
    /// thread's and moves with it.
    call_frames: Vec<CallFrame>,
    /// The guest's threads, one of which the CPU is on. A payload with a single
    /// thread never leaves thread 0, and then nothing here changes what the run
    /// loop does.
    threads: Threads,
    /// The instruction count at which the running thread is given up to another
    /// thread that wants the CPU, when there is one.
    slice_at: u64,
    /// The instruction count when the running thread was entered, so the dump a
    /// stalled run prints can attribute the instructions to the right thread.
    thread_start: u64,
    /// Whether a stall has been reported, so an embedder's log gets one line
    /// about it however many times the guest spins back into the same state.
    stall_reported: bool,
    /// An address to report every time the guest reaches it, for a run that has
    /// to be read at one exact instruction rather than sampled. Set by a
    /// diagnostic harness, never by the page.
    pub watch: Option<u32>,
    watch_hits: u32,
    /// The last few arrivals at [`Self::watch`], printed when the run dies: the
    /// state that led to a fault, which sampling cannot show.
    watch_recent: std::collections::VecDeque<String>,
    /// The instructions that ran just before a fault, when `SHIMKIT_TRACE_TAIL`
    /// asks for them. A wrap that sampling sees as one step is a sequence here.
    tail: std::collections::VecDeque<String>,
    /// How many instructions [`Self::tail`] keeps, or 0 for none.
    tail_len: usize,
    /// The instruction count at which [`Self::tail`] starts recording, so a run
    /// that takes billions of instructions does not pay for the trace until the
    /// part that matters.
    tail_from: u64,
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

/// How long one thread may keep the CPU before another thread that wants it gets
/// a turn, in instructions.
///
/// Every other switch in this host happens where the guest asked for one -- a
/// wait, a sleep, a signal -- and that is still all a guest needs for the usual
/// producer/consumer handshake. It is not enough for the other shape a Windows
/// game is written in: a main thread that spins on a flag its worker sets, with
/// no call to the host anywhere in the loop. There is no switch point in that
/// loop to take, so the worker would never run, and the spinning thread would
/// give up on a flag that was only ever late. Windows takes the CPU away at a
/// timer tick, so this does the same at an instruction count -- and only when
/// there is another thread that can run, which is why a single-threaded payload
/// behaves exactly as it did before threads existed.
const THREAD_SLICE: u64 = 200_000;

/// What [`Session::serve`] did with a host call.
enum Served {
    /// The guest continues with this value in `eax`.
    Return(u32),
    /// The guest asked to terminate.
    Exit(u32),
    /// The guest is now running a routine the shim asked for; `eax` is the
    /// routine's to fill.
    Call,
    /// The thread that made the call handed the CPU over -- a wait, a sleep or its
    /// own end -- and the scheduler picks what runs next at the top of the loop.
    Switched,
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
        // Read before the image becomes guest memory: a resource's bytes live in
        // the mapped `.rsrc`, but the tree that names them is only read here.
        let resources = image.resources()?;
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
        host.resources = resources;
        // A CRT variable is not a function: `_acmdln`'s slot holds the address of
        // a pointer the guest dereferences, not a stub to call. `npos` in the C++
        // runtime is the same shape.
        shim::crt::fill_data_slots(&mut host, &mut memory, target.table)?;
        shim::msvcp::fill_data_slots(&mut host, &mut memory, target.table)?;
        // A device the browser cannot have is supplied as a name and a length:
        // the guest measures the file and never reads it. The names come from
        // the target, so no product's disc layout lives in this crate.
        for (name, size) in target.provided_files {
            host.files.provide_sized(name, *size);
            host.notice(format!(
                "supplied marker: {name} ({size} bytes) stands in for a device the browser cannot have"
            ));
        }
        // The thread the loader enters `WinMainCRTStartup` on, which is the only
        // one until the guest asks for another.
        let mut threads = Threads::new();
        threads.start(cpu.regs, cpu.fpu.clone());
        Ok(Self {
            target,
            bindings: target
                .table
                .iter()
                .map(|slot| shim::binding(target.shims, slot))
                .collect(),
            memory,
            cpu,
            decodes: cpu::Decodes::new(),
            host,
            host_calls: 0,
            host_returns: 0,
            last_import: None,
            timer_frame: None,
            timer_started: None,
            timer_instructions: 0,
            timer_by_id: [0; 16],
            call_frames: Vec::new(),
            threads,
            slice_at: THREAD_SLICE,
            thread_start: 0,
            stall_reported: false,
            watch: None,
            watch_hits: 0,
            watch_recent: Default::default(),
            tail: Default::default(),
            tail_len: std::env::var("SHIMKIT_TRACE_TAIL")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
            tail_from: std::env::var("SHIMKIT_TRACE_FROM")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
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

    /// Every guest thread as it stands: where it is, what it is parked on, and
    /// how many instructions it has retired.
    ///
    /// A run that neither faults nor presents anything is read from here. The
    /// thread the CPU is on has not been accounted yet, so the slice in progress
    /// is added to it; the counts then add up to [`Session::instructions`].
    pub fn thread_summaries(&self) -> Vec<ThreadSummary> {
        let mut waits: std::collections::HashMap<u32, Vec<u32>> =
            self.host.objects.waiting().into_iter().collect();
        let current = self.threads.current_id();
        self.threads
            .all()
            .iter()
            .map(|thread| {
                let running = thread.id == current;
                let in_progress = if running {
                    self.cpu.instructions.saturating_sub(self.thread_start)
                } else {
                    0
                };
                ThreadSummary {
                    id: thread.id,
                    state: thread.state,
                    pc: thread.regs.eip,
                    instructions: thread.instructions + in_progress,
                    waits: waits.remove(&thread.id).unwrap_or_default(),
                    stack: self.frames_from(thread.regs.ebp, 8),
                    current: running,
                }
            })
            .collect()
    }

    /// One line for each arrival at [`Self::watch`], with the registers and the
    /// eight stack words a caller pushed. An argument is that stack word, which
    /// is how a call site says what it passes without being traced.
    fn report_watch(&mut self) {
        self.watch_hits += 1;
        let registers = self
            .cpu
            .registers()
            .iter()
            .map(|(name, value)| format!("{name}={value:#010x}"))
            .collect::<Vec<_>>()
            .join(" ");
        let stack = (0..8)
            .map(|index| {
                format!(
                    "{:#010x}",
                    self.memory
                        .read_u32(self.cpu.regs.esp.wrapping_add(4 * index))
                        .unwrap_or(0)
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let line = format!(
            "watch {:#010x} at {}: {registers} zf={} cf={}\n  stack+0..8: {stack}",
            self.cpu.regs.eip,
            self.cpu.instructions,
            u8::from(self.cpu.regs.flags.zf),
            u8::from(self.cpu.regs.flags.cf)
        );
        if self.watch_hits <= 64 {
            println!("{line}");
        }
        self.watch_recent.push_back(line);
        if self.watch_recent.len() > 64 {
            self.watch_recent.pop_front();
        }
    }

    /// The return addresses on the guest's frame chain, innermost first.
    ///
    /// A guest that spins without calling the host is otherwise a mystery: the
    /// chain names the routine that is spinning and the calls that led to it,
    /// which no import name can.
    pub fn call_stack(&self, depth: usize) -> Vec<u32> {
        self.frames_from(self.cpu.regs.ebp, depth)
    }

    /// The frame chain that starts at `ebp`, innermost first. Every thread has
    /// one, which is what lets a stalled one name the routine it is in.
    fn frames_from(&self, mut frame: u32, depth: usize) -> Vec<u32> {
        let mut stack = Vec::new();
        for _ in 0..depth {
            if frame == 0 {
                break;
            }
            let (Ok(saved), Ok(returned)) = (
                self.memory.read_u32(frame),
                self.memory.read_u32(frame.wrapping_add(4)),
            ) else {
                break;
            };
            if returned == 0 {
                break;
            }
            stack.push(returned);
            // The chain climbs the stack; a frame that does not is not a frame.
            if saved <= frame {
                break;
            }
            frame = saved;
        }
        stack
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
            // The thread the CPU is on may have parked itself at the end of the
            // host call that just completed, or given way to another one, so a
            // thread that can run takes over here. With one thread this is one
            // compare per instruction and there is never anyone else to pick.
            if self.threads.switch_wanted() {
                if let Some(outcome) = self.schedule() {
                    return outcome;
                }
            } else if self.threads.len() > 1 && self.cpu.instructions >= self.slice_at {
                // The quantum. A thread that is spinning on something another
                // thread is meant to set has no switch point of its own, so the
                // CPU is taken away here the way an operating system takes it.
                //
                // The switch is taken now, not after one more instruction: the
                // parked record holds the instruction that has not run yet, so
                // running it here would run it twice when the thread is entered
                // again -- a `dec` of a loop counter executes twice, skips zero
                // and turns the loop into a march through unmapped memory.
                self.slice_at = self.cpu.instructions + THREAD_SLICE;
                self.park(ThreadWant::Yield);
                continue;
            }
            // Both trampolines live above the host-call base, and no guest code
            // does, so one compare keeps the two checks out of the common path.
            let eip = self.cpu.regs.eip;
            if self.watch == Some(eip) {
                self.report_watch();
            }
            if eip >= cpu::HOST_FUNCTION_BASE {
                if eip == cpu::THREAD_TRAMPOLINE {
                    // A `CreateThread` routine returned, which is this thread
                    // asking to be ended: there was never an import under it.
                    self.end_thread(self.cpu.regs.eax);
                    continue;
                }
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
                    // The routine's result is the import's return value -- unless
                    // the shim said what the import must answer -- and the import's
                    // frame, arguments still on the stack, is restored.
                    let result = frame.answer.unwrap_or(self.cpu.regs.eax);
                    self.cpu.regs = frame.regs;
                    self.cpu.fpu = frame.fpu;
                    // A shim that asked to be resumed runs again here, with the
                    // routine's result available to it, and decides whether the
                    // import is finished or the next entry is needed.
                    if let Some(binding) = frame.resume {
                        self.host.guest_result = result;
                        self.host.resumed = true;
                        self.host.this = self.cpu.regs.ecx;
                        let mut arguments = shim::Call {
                            memory: &mut self.memory,
                            args: frame.args,
                            arg_bytes: binding.arg_bytes,
                        };
                        let action = (binding.shim)(&mut self.host, &mut arguments);
                        self.host.resumed = false;
                        let action = match action {
                            Ok(action) => action,
                            Err(error) => return Outcome::Fault(error),
                        };
                        let popped = if shim::is_cdecl(binding.dll, binding.name) {
                            0
                        } else {
                            binding.arg_bytes
                        };
                        match self.finish(action, frame.args, popped, Some(binding)) {
                            Ok(Served::Call | Served::Switched) => continue,
                            Ok(Served::Return(value)) => {
                                self.cpu.regs.eax = value;
                                continue;
                            }
                            Ok(Served::Exit(code)) => {
                                self.host.exit_code = Some(code);
                                return Outcome::Halted;
                            }
                            Err(error) => return Outcome::Fault(error),
                        }
                    }
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
            if self.tail_len != 0 && self.cpu.instructions >= self.tail_from {
                let regs = &self.cpu.regs;
                self.tail.push_back(format!(
                    "{:#012x} eip={:#010x} esp={:#010x} ebp={:#010x} eax={:#010x} ebx={:#010x} ecx={:#010x} edx={:#010x} esi={:#010x} edi={:#010x} zf={} lf={}",
                    self.cpu.instructions,
                    regs.eip,
                    regs.esp,
                    regs.ebp,
                    regs.eax,
                    regs.ebx,
                    regs.ecx,
                    regs.edx,
                    regs.esi,
                    regs.edi,
                    u8::from(regs.flags.zf),
                    u8::from(regs.flags.sf != regs.flags.of),
                ));
                if self.tail.len() > self.tail_len {
                    self.tail.pop_front();
                }
            }
            match cpu::step(&mut self.memory, &mut self.cpu, &mut self.decodes) {
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
                        // The thread handed the CPU over; the top of the loop
                        // picks who runs next.
                        Ok(Some(Served::Switched)) => {}
                        // No shim yet: this is the diagnostic, not a failure.
                        Ok(None) => return Outcome::HostCall(call),
                        Err(error) => return self.fault(error),
                    }
                }
                Err(error) => return self.fault(error),
            }
        }
        Outcome::Steps
    }

    /// A fault that names the thread it happened on. Which thread a fault belongs
    /// to is the first thing to know about it once the guest runs more than one,
    /// and a single-threaded guest is not told, because it has only one.
    fn fault(&self, error: String) -> Outcome {
        self.dump_tail();
        // The arrivals at a watched address that came just before the fault, so
        // the call that produced a bad value is in the log rather than guessable.
        for line in &self.watch_recent {
            println!("{line}");
        }
        if self.threads.len() > 1 {
            Outcome::Fault(format!("thread {}: {error}", self.threads.current_id()))
        } else {
            Outcome::Fault(error)
        }
    }

    /// Prints the instructions [`Self::tail`] kept. An embedder calls this when
    /// a run ends however it ends, because a guest that leaves through a shim --
    /// `_purecall` exiting, for one -- never reaches [`Self::fault`].
    pub fn dump_tail(&self) {
        for line in &self.tail {
            println!("{line}");
        }
    }

    /// Prints the last arrivals at [`Self::watch`]. A run that ends without a
    /// fault still needs them: which addresses the guest reaches at the *end* is
    /// what says whether it is looping inside the work it started with.
    pub fn dump_watch(&self) {
        for line in &self.watch_recent {
            println!("{line}");
        }
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
        if std::env::var("SHIMKIT_LOG_TIMERS").is_ok() && self.host.notices.len() < 600 {
            self.host.notice(format!(
                "timer #{id} fires at {now} ms, callback {callback:#010x}"
            ));
        }
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
        // A name the guest resolved with `GetProcAddress` and called: the binding
        // is the host's own, and its declared signature is what the stack is
        // popped by, since there was never an import slot to record one.
        if let Some(index) = call.proc {
            let Some(binding) = self.host.procs.get(index).copied() else {
                return Err(format!(
                    "call to the resolved name {index} (address {:#010x}), which this host never handed out",
                    call.iat_va
                ));
            };
            self.host.this = self.cpu.regs.ecx;
            let mut arguments = shim::Call {
                memory: &mut self.memory,
                args: call.args,
                arg_bytes: binding.arg_bytes,
            };
            let action = (binding.shim)(&mut self.host, &mut arguments)
                .map_err(|error| named(binding, &error))?;
            let popped = if shim::is_cdecl(binding.dll, binding.name) {
                0
            } else {
                binding.arg_bytes
            };
            self.last_import = Some(format!(
                "{} (resolved) args={:#010x} eax={:#010x}",
                binding.name,
                call.args,
                action_value(action)
            ));
            self.log_call(call.return_address);
            return self.finish(action, call.args, popped, Some(binding)).map(Some);
        }
        // A call through a host object carries its own interface and method, and
        // the argument count comes from the same table the step loop used.
        if let Some((kind, method)) = call.thunk {
            let arg_bytes = crate::thunks::arg_bytes(kind, method);
            self.host.this = self.cpu.regs.ecx;
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
                action_value(action)
            ));
            self.log_call(call.return_address);
            return self.finish(action, call.args, arg_bytes, None).map(Some);
        }
        let Some(slot) = self.target.table.slots.get(call.slot) else {
            return Err(format!("slot {} is outside the dispatch table", call.slot));
        };
        let Some(binding) = self.bindings.get(call.slot).copied().flatten() else {
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

        // `__thiscall` passes `this` in `ecx`, which is the only way a C++ member
        // function can be served: the shim reads it from here.
        self.host.this = self.cpu.regs.ecx;
        let mut arguments = shim::Call {
            memory: &mut self.memory,
            args: call.args,
            arg_bytes: binding.arg_bytes,
        };
        // A shim that fails names the import it failed on: the address a fault
        // reports is the guest's, which on its own does not say whose fault it is.
        let action =
            (binding.shim)(&mut self.host, &mut arguments).map_err(|error| named(binding, &error))?;
        // A cdecl import leaves its arguments for the caller's own `add esp`.
        let popped = if shim::is_cdecl(slot.dll, &slot.name) {
            0
        } else {
            binding.arg_bytes
        };
        self.last_import = Some(format!(
            "{} args={:#010x} eax={:#010x}",
            slot.label(),
            call.args,
            action_value(action)
        ));
        self.log_call(call.return_address);
        self.finish(action, call.args, popped, Some(binding)).map(Some)
    }

    /// Prints one host call under `SHIMKIT_LOG_CALLS`, with the address of the
    /// guest routine that asked for it.
    ///
    /// A name and an argument count say which import a game is stuck on, but not
    /// which of its routines asked, which is what a disassembly is read from. The
    /// address comes from the `call` itself: this host does not push a return
    /// address for an import, so the stack below the arguments is not one.
    /// Interface methods go through the same line as imports: a draw that never
    /// happens is read from the calls around it, not from the import names alone.
    fn log_call(&self, return_address: u32) {
        if std::env::var("SHIMKIT_LOG_CALLS").is_err() {
            return;
        }
        // A run of billions of instructions cannot have every call printed, and
        // the calls that matter are the ones around the stop. `SHIMKIT_LOG_CALLS_AFTER`
        // starts the log at that instruction count.
        if let Some(after) = std::env::var("SHIMKIT_LOG_CALLS_AFTER")
            .ok()
            .and_then(|text| text.parse::<u64>().ok())
        {
            if self.cpu.instructions < after {
                return;
            }
        }
        println!(
            "{:14} t{} {:#010x} {}",
            self.cpu.instructions,
            self.host.current_thread,
            return_address,
            self.last_import.as_deref().unwrap_or_default()
        );
    }

    /// Completes a host call whose shim has run.
    ///
    /// A shim that needs guest code leaves a [`GuestCall`] in the host instead of
    /// returning a value: the routine is entered here with its arguments pushed
    /// and `CALL_TRAMPOLINE` as its return address, the import's own frame is
    /// suspended under it, and the value the routine leaves in `eax` finishes the
    /// import when the trampoline is reached.
    ///
    /// `resumable` is the import the shim belongs to, when it asked to run again
    /// after the routine returns -- a table walk needs one entry per pass.
    fn finish(
        &mut self,
        action: Action,
        args: u32,
        arg_bytes: u32,
        resumable: Option<&'static shim::Binding>,
    ) -> Result<Served> {
        // Whatever this shim signalled is released first, before any of the ways
        // below can return: a thread that signalled something while another
        // routine was being run for it still has to be woken.
        self.release_woken();
        if let Some(request) = self.host.guest_call.take() {
            let resume = if request.resume { resumable } else { None };
            self.enter_guest_call(request, args, arg_bytes, resume)?;
            return Ok(Served::Call);
        }
        // A thread the shim asked for. It is created here -- its stack is guest
        // memory -- and it runs as soon as this call finishes, because that is
        // what the guest assumes: the code after `CreateThread` returns may count
        // on the new thread having started.
        let started = match self.host.spawn.take() {
            Some(spawn) => {
                self.start_thread(spawn)?;
                true
            }
            None => false,
        };
        // stdcall: the callee pops its arguments.
        self.cpu.regs.esp = args.wrapping_add(arg_bytes);
        let (served, value) = match action {
            Action::Return(value) => (Served::Return(value), Some(value)),
            Action::Return64(value) => {
                // A 64-bit result is `edx:eax`, and a caller that ignores the
                // high half must still see it zeroed rather than whatever the
                // previous call left.
                self.cpu.regs.edx = (value >> 32) as u32;
                (Served::Return(value as u32), Some(value as u32))
            }
            Action::Exit(code) => (Served::Exit(code), None),
            Action::Float(op) => {
                let (value, high) = self.cpu.fpu.apply(op);
                // `_ftol` answers in `edx:eax`, and a `double` return leaves the
                // high word zero because the caller only reads `eax` when it does
                // not take the float stack.
                self.cpu.regs.edx = high;
                (Served::Return(value), Some(value))
            }
        };
        // A shim that parked the calling thread stops it here. The value the shim
        // returned is what that thread sees when it runs again, unless the wait it
        // parked in has a result of its own.
        if let Some(want) = self.host.thread_want.take() {
            if let Some(value) = value {
                self.cpu.regs.eax = value;
            }
            self.park(want);
            return Ok(Served::Switched);
        }
        if started {
            // The thread that was just created gets the CPU, and this one waits
            // its turn: `CreateThread` returning is the guest asking for that
            // thread to be started.
            if let Some(value) = value {
                self.cpu.regs.eax = value;
            }
            self.park(ThreadWant::Yield);
            return Ok(Served::Switched);
        }
        Ok(served)
    }

    /// Wakes the threads a shim released by signalling, releasing or ending
    /// something. Their own waits decide whether they are awake yet.
    fn release_woken(&mut self) {
        for (id, answer) in std::mem::take(&mut self.host.woken) {
            self.threads.wake(id, answer);
        }
    }

    /// Creates a guest thread: a stack of its own in guest memory, the routine's
    /// parameter pushed under a return address the run loop recognises, and the
    /// register set the scheduler enters it with.
    ///
    /// Nothing runs here. The new thread is runnable from now on, and the caller
    /// gives way to it as soon as this import finishes.
    fn start_thread(&mut self, spawn: Spawn) -> Result<()> {
        const DEFAULT_STACK: u32 = 0x0004_0000;
        let size = spawn
            .stack_size
            .unwrap_or(DEFAULT_STACK)
            .clamp(0x4000, crate::memory::STACK_SIZE);
        let base = self.host.allocate(size, 16)?;
        let mut esp = base + size;
        // `ThreadProc(parameter)`, entered the way Windows enters it: the argument
        // pushed, then the address that ends the thread when the routine returns.
        for value in [spawn.parameter, cpu::THREAD_TRAMPOLINE] {
            esp = esp.wrapping_sub(4);
            self.memory.write_u32(esp, value)?;
        }
        let regs = cpu::Registers {
            eip: spawn.entry,
            esp,
            ..cpu::Registers::default()
        };
        self.threads
            .spawn(spawn.id, spawn.handle, regs, cpu::Fpu::default());
        if let Some(thread) = self.threads.thread_mut(spawn.id) {
            thread.stack = base;
        }
        self.host.notice(format!(
            "CreateThread at {:#010x} starts thread {} with {size} bytes of stack ({} thread(s))",
            spawn.entry,
            spawn.id,
            self.threads.len()
        ));
        Ok(())
    }

    /// Ends the running thread: `ExitThread`, or a `CreateThread` routine returning
    /// to the thread trampoline. Its handle is signalled, so a join returns.
    fn end_thread(&mut self, code: u32) {
        let id = self.threads.current_id();
        // A thread that is over holds no call frames and no callback.
        self.call_frames.clear();
        self.timer_frame = None;
        self.timer_started = None;
        let (regs, fpu) = (self.cpu.regs, self.cpu.fpu.clone());
        self.threads.park(regs, fpu, &ThreadWant::Exit(code));
        self.release_stack();
        for handle in self.host.objects.thread_exited(id, code) {
            self.host.woken.extend(self.host.objects.wake(handle));
        }
        self.release_woken();
    }

    /// Hands a dead thread's stack back to the arena. The main thread's is not
    /// the arena's to take, and is marked with zero.
    fn release_stack(&mut self) {
        let stack = self.threads.current_thread_mut().stack;
        if stack != 0 {
            self.threads.current_thread_mut().stack = 0;
            self.host.release(stack);
        }
    }

    /// Parks the running thread: its machine state moves into its own record and
    /// the scheduler picks what runs next at the top of the loop.
    fn park(&mut self, want: ThreadWant) {
        let id = self.threads.current_id();
        // The host calls suspended on this thread's stack, and a timer callback
        // that interrupted it, belong to it rather than to the session.
        let thread = self.threads.current_thread_mut();
        std::mem::swap(&mut self.call_frames, &mut thread.frames);
        std::mem::swap(&mut self.timer_frame, &mut thread.timer_frame);
        std::mem::swap(&mut self.timer_started, &mut thread.timer_started);
        let (regs, fpu) = (self.cpu.regs, self.cpu.fpu.clone());
        self.threads.park(regs, fpu, &want);
        match want {
            ThreadWant::Wait { timeout: Some(ms) } => {
                // The deadline is the guest's own clock: the embedder's while it
                // supplies one, and the instruction count when it does not.
                let now = self.host.guest_time_ms(self.cpu.instructions);
                self.host.objects.set_deadline(id, now + u64::from(ms));
                if std::env::var("SHIMKIT_LOG_TIMEOUTS").is_ok() {
                    println!("deadline thread {id} at {now} until {}", now + u64::from(ms));
                }
            }
            ThreadWant::Exit(code) => {
                self.release_stack();
                for handle in self.host.objects.thread_exited(id, code) {
                    self.host.woken.extend(self.host.objects.wake(handle));
                }
                self.release_woken();
            }
            ThreadWant::Wait { timeout: None } | ThreadWant::Yield => {}
        }
    }

    /// Moves the CPU onto a thread: its registers, its x87 stack, the host calls
    /// and timer callback suspended on its stack, and its thread-local slots.
    fn enter(&mut self, index: usize) {
        // The thread being left keeps the instructions it retired, which is what
        // makes a stuck thread's share of the run visible.
        let spent = self.cpu.instructions.saturating_sub(self.thread_start);
        self.threads.current_thread_mut().instructions += spent;
        self.thread_start = self.cpu.instructions;
        let (id, regs, fpu) = self.threads.run_on(index);
        self.cpu.regs = regs;
        self.cpu.fpu = fpu;
        // Every thread is entered with a whole quantum: a thread that was
        // switched in at the end of someone else's slice has not had its own yet.
        self.slice_at = self.cpu.instructions + THREAD_SLICE;
        if let Some(answer) = self.threads.current_thread_mut().answer.take() {
            self.cpu.regs.eax = answer;
        }
        let thread = self.threads.current_thread_mut();
        std::mem::swap(&mut self.call_frames, &mut thread.frames);
        std::mem::swap(&mut self.timer_frame, &mut thread.timer_frame);
        std::mem::swap(&mut self.timer_started, &mut thread.timer_started);
        self.host.current_thread = id;
        self.host.tls.switch_to(id);
        if std::env::var("SHIMKIT_LOG_THREADS").is_ok() {
            println!(
                "{:14} thread {id} on at {:#010x}",
                self.cpu.instructions, self.cpu.regs.eip
            );
        }
    }

    /// Hands the CPU to a thread that can run, answering why the run has to stop
    /// when none can.
    fn schedule(&mut self) -> Option<Outcome> {
        // A timed wait whose deadline has passed answers itself first: no more of
        // that time is coming, and the guest should not have to care that the CPU
        // was somewhere else while it went by.
        let now = self.host.guest_time_ms(self.cpu.instructions);
        let expired = self.host.objects.expire(now);
        if std::env::var("SHIMKIT_LOG_TIMEOUTS").is_ok() && !expired.is_empty() {
            println!("expired at {now}: {expired:?}");
        }
        for (id, answer) in expired {
            self.threads.wake(id, answer);
        }
        if let Some(index) = self.threads.schedule() {
            self.enter(index);
            return None;
        }
        if self.threads.all_exited() {
            return Some(Outcome::Halted);
        }
        // Every thread is parked. Nothing this host can do will release them, so
        // the waits are answered rather than left to hang the page.
        self.force_waits();
        match self.threads.schedule() {
            Some(index) => {
                self.enter(index);
                None
            }
            None => Some(Outcome::Steps),
        }
    }

    /// Answers every parked wait, because nothing can run and nothing will.
    ///
    /// Windows would sit here for as long as it took, which a page with a frame
    /// budget cannot. Before this host ran threads at all, every wait was answered
    /// the moment it was asked, so this is that behaviour held in reserve: the
    /// guest goes on with the result the old host would have given it, and one
    /// notice names what was waiting, because that is the evidence a missing
    /// signal is found from.
    fn force_waits(&mut self) {
        let parked = self.host.objects.waiting();
        if parked.is_empty() {
            return;
        }
        if std::env::var("SHIMKIT_LOG_STALLS").is_ok() {
            println!(
                "{:14} stall: {:?}",
                self.cpu.instructions,
                parked.iter()
                    .map(|(id, handles)| (*id, handles.clone()))
                    .collect::<Vec<_>>()
            );
        }
        if !self.stall_reported {
            self.stall_reported = true;
            let who = parked
                .iter()
                .map(|(id, handles)| {
                    if handles.is_empty() {
                        format!("thread {id} is asleep")
                    } else {
                        let list: Vec<String> =
                            handles.iter().map(|handle| format!("{handle:#010x}")).collect();
                        format!("thread {id} waits on {}", list.join(", "))
                    }
                })
                .collect::<Vec<_>>()
                .join("; ");
            self.host.notice(format!(
                "every thread is parked ({who}) and nothing can release them, so the waits were \
                 answered to let the guest go on"
            ));
        }
        for (id, _) in parked {
            self.host.objects.forget(id);
            self.threads.wake(id, WAIT_OBJECT_0);
        }
    }

    /// Pushes a guest routine's arguments and enters it, saving the frame the
    /// import was called from.
    ///
    /// A shim asks for this by leaving a [`GuestCall`] in the host rather than by
    /// calling it; it is public so a port can drive a routine directly when it
    /// needs to probe the guest.
    pub fn start_guest_call(&mut self, request: GuestCall, args: u32, arg_bytes: u32) -> Result<()> {
        self.enter_guest_call(request, args, arg_bytes, None)
    }

    /// The same entry, with the import to run again when the routine returns.
    fn enter_guest_call(
        &mut self,
        request: GuestCall,
        args: u32,
        arg_bytes: u32,
        resume: Option<&'static shim::Binding>,
    ) -> Result<()> {
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
            answer: request.answer,
            resume,
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

/// A shim's error, prefixed with the import it came from.
fn named(binding: &shim::Binding, error: &str) -> String {
    format!("{}!{}: {error}", binding.dll, binding.name)
}

/// The value the trace prints for an action. A float result lives on the x87
/// stack rather than in `eax`, so the line names the import and leaves the value
/// out instead of guessing at one.
fn action_value(action: Action) -> u32 {
    match action {
        Action::Return(value) => value,
        Action::Return64(value) => value as u32,
        Action::Exit(code) => code,
        Action::Float(_) => 0,
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

