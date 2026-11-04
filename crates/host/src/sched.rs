//! Guest threads, run one at a time.
//!
//! A guest that calls `CreateThread` gets a thread this host runs itself: a stack
//! in guest memory and a register set of its own, entered only when the thread
//! that is running cannot go on. Almost every switch happens where the guest
//! asked for one -- a wait, a sleep, a semaphore release, a thread it just
//! started -- which is what makes this tractable in a host with no OS under it.
//! The one exception is the quantum in [`crate::session`]: a game that spins on a
//! flag its worker sets has no switch point in that loop, so the CPU is taken
//! away at an instruction count the way an operating system takes it at a timer
//! tick, and only ever to a thread that already wants it.
//!
//! The guest's own locks are the shims' business, not this module's:
//! `EnterCriticalSection` parks the second thread on the lock and `Leave` hands
//! it to the waiter that has been waiting longest, with the recursion count a
//! re-entrant enter needs (`kernel32.rs`); the CRT's `_lock` and `_Lockit` go
//! through the same objects. What nothing here does is *contend* -- no thread
//! ever has a lock taken away from it -- so the exclusion a guest sees is only
//! as strong as its own discipline about releasing. A guest that hands work
//! over with a semaphore, which is what a Win32 game's worker usually does, and
//! one that guards its queues with critical sections both get the behaviour
//! they were written against.
//!
//! The machine state a thread carries is exactly what a swap of the CPU needs:
//! registers, the x87 stack (games keep audio state there across a yield),
//! the host calls suspended on its own stack, and a timer callback or a queued
//! `ReadFileEx` completion that interrupted it. The wait a thread is parked in
//! lives in [`crate::objects`], which decides what a signal answers; this
//! module only decides who runs.

use crate::cpu::{Fpu, Registers};
use crate::shim::Binding;

/// What a shim asked the session to do with the thread that is running.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ThreadWant {
    /// Park until the wait in [`crate::objects`] completes, or until `timeout`
    /// guest milliseconds have passed.
    Wait { timeout: Option<u32> },
    /// Let another runnable thread go first, but do not stop: `Sleep`.
    Yield,
    /// End the running thread with this exit code.
    Exit(u32),
}

/// A thread `CreateThread` asked for, as the shim left it. The session makes the
/// stack and builds the register set, because both need guest memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub id: u32,
    pub handle: u32,
    pub entry: u32,
    pub parameter: u32,
    /// `CreateThread`'s stack size, when the guest asked for one.
    pub stack_size: Option<u32>,
}

/// Whether a thread can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Runnable,
    /// Parked on a wait in [`crate::objects`], or asleep.
    Waiting,
    Exited,
}

/// A host call suspended while the guest runs a routine for it: a window
/// procedure a `DispatchMessageA` started, with the import's own frame under it.
/// It belongs to the thread that asked, because that thread's stack is what it
/// is on.
pub struct CallFrame {
    /// Everything but `eax`, which the routine's return value replaces.
    pub regs: Registers,
    pub fpu: Fpu,
    /// Where the import's own arguments start, so the call can be popped when the
    /// routine returns.
    pub args: u32,
    pub arg_bytes: u32,
    /// The value the import returns instead of the routine's own, when the shim
    /// said so: `LoadLibraryA` answers with a module handle, not `DllMain`'s.
    pub answer: Option<u32>,
    /// The import to run again when the routine returns, when the shim asked to be
    /// resumed (`_initterm` and `exit` walk a list one entry per pass). The
    /// routine's frame was pushed over the import's arguments, so a resumed shim
    /// reads its state from the host instead.
    pub resume: Option<&'static Binding>,
}

/// One guest thread.
pub struct Thread {
    pub id: u32,
    pub handle: u32,
    pub state: State,
    /// `SuspendThread`/`ResumeThread` nesting depth. A thread with a nonzero
    /// count never runs, however it was parked: suspension is orthogonal to
    /// waiting, and waking a suspended waiter only leaves it runnable once
    /// the count is resumed back down. Hercules stages video on a worker
    /// into the buffer the main thread decodes out of, and the two take
    /// turns with exactly this count -- without it the worker laps the
    /// decoder and the decoder walks a table the next stage overwrote.
    pub suspends: u32,
    pub regs: Registers,
    pub fpu: Fpu,
    /// Host calls suspended on this thread's own stack.
    pub frames: Vec<CallFrame>,
    /// The frame a timer callback interrupted on this thread, and the count and
    /// timer it started at.
    pub timer_frame: Option<(Registers, Fpu)>,
    pub timer_started: Option<(u64, u32)>,
    /// The frame a queued `ReadFileEx` completion interrupted on this thread,
    /// and the instruction count it started at. Same shape as a timer's, and
    /// the two never run at once.
    pub apc_frame: Option<(Registers, Fpu)>,
    pub apc_started: Option<u64>,
    /// What the import this thread parked in answers with, when that is not the
    /// value the shim already returned: a wait knows its result only when it
    /// completes, which is after the shim has run.
    pub answer: Option<u32>,
    /// Set by [`ThreadWant::Yield`]: this thread has had its turn and should wait
    /// for the others before running again.
    pub yielded: bool,
    /// Base of this thread's stack in guest memory, which goes back to the arena
    /// when the thread ends. Zero for the thread the session starts on, whose
    /// stack is the image's and not the arena's.
    pub stack: u32,
    /// Instructions this thread retired before the CPU was last taken away from
    /// it, for the dump a run that neither faults nor moves prints.
    pub instructions: u64,
    /// Whether this thread's registers and call frames are in its record as they
    /// stand. Set by [`Threads::park`], cleared by [`Threads::run_on`], because a
    /// thread that is running has left them behind. A switch away from a thread
    /// that is not saved is a switch that loses its place: the record still holds
    /// the context the thread was spawned with, and it resumes at its entry point
    /// instead of where it stopped.
    pub saved: bool,
}

/// One thread as a diagnostic sees it: where it stopped, what it is parked on,
/// and how much of the run it has spent.
#[derive(Clone, Debug)]
pub struct ThreadSummary {
    pub id: u32,
    pub state: State,
    pub pc: u32,
    /// Instructions retired on this thread, including the slice it is in the
    /// middle of when it is the current one.
    pub instructions: u64,
    /// Handles the thread is parked on; empty for a sleep or a runnable thread.
    pub waits: Vec<u32>,
    /// The guest return addresses on this thread's frame chain, innermost
    /// first, so the routine it is stuck in can be named.
    pub stack: Vec<u32>,
    pub current: bool,
}

impl Thread {
    fn new(id: u32, handle: u32, regs: Registers, fpu: Fpu) -> Self {
        Thread {
            id,
            handle,
            state: State::Runnable,
            suspends: 0,
            regs,
            fpu,
            frames: Vec::new(),
            timer_frame: None,
            timer_started: None,
            apc_frame: None,
            apc_started: None,
            answer: None,
            yielded: false,
            stack: 0,
            instructions: 0,
            saved: true,
        }
    }

    pub fn running(&self) -> bool {
        self.state == State::Runnable && self.suspends == 0
    }
}

/// Every thread the guest has, and which one the CPU is on.
pub struct Threads {
    threads: Vec<Thread>,
    current: usize,
}

impl Threads {
    pub fn new() -> Self {
        Threads {
            threads: Vec::new(),
            current: 0,
        }
    }

    /// Registers the thread the session's own CPU starts on: id 0, handle 0.
    pub fn start(&mut self, regs: Registers, fpu: Fpu) -> u32 {
        self.threads.push(Thread::new(0, 0, regs, fpu));
        0
    }

    /// Adds a thread that has not run yet. Its registers are already at the entry
    /// point with its stack under them.
    pub fn spawn(&mut self, id: u32, handle: u32, regs: Registers, fpu: Fpu) {
        self.threads.push(Thread::new(id, handle, regs, fpu));
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn len(&self) -> usize {
        self.threads.len()
    }

    pub fn current_id(&self) -> u32 {
        self.threads[self.current].id
    }

    /// Whether the thread the CPU is on can run.
    pub fn running(&self) -> bool {
        self.threads[self.current].running()
    }

    /// Whether the CPU should look for another thread before the next
    /// instruction: the running one cannot go on, or it just gave way to the
    /// others.
    pub fn switch_wanted(&self) -> bool {
        let thread = &self.threads[self.current];
        !thread.running() || thread.yielded
    }

    pub fn current_thread(&self) -> &Thread {
        &self.threads[self.current]
    }

    pub fn current_thread_mut(&mut self) -> &mut Thread {
        &mut self.threads[self.current]
    }

    /// Every thread, in the order it was created.
    pub fn all(&self) -> &[Thread] {
        &self.threads
    }

    pub fn thread_mut(&mut self, id: u32) -> Option<&mut Thread> {
        self.threads.iter_mut().find(|thread| thread.id == id)
    }

    pub fn all_exited(&self) -> bool {
        self.threads.iter().all(|thread| thread.state == State::Exited)
    }

    /// Saves the running thread where it stopped and marks why.
    ///
    /// The registers are the ones the import left: `eip` already past the call,
    /// `esp` with the arguments popped. A wait's own result arrives later, in
    /// [`Threads::wake`], because it is not known until the object is signalled.
    pub fn park(&mut self, regs: Registers, fpu: Fpu, want: &ThreadWant) -> &Thread {
        let thread = &mut self.threads[self.current];
        thread.regs = regs;
        thread.fpu = fpu;
        thread.saved = true;
        match want {
            ThreadWant::Wait { .. } => {
                thread.state = State::Waiting;
                thread.answer = None;
            }
            ThreadWant::Yield => {
                thread.state = State::Runnable;
                thread.yielded = true;
            }
            ThreadWant::Exit(code) => {
                thread.state = State::Exited;
                thread.answer = Some(*code);
            }
        }
        &self.threads[self.current]
    }

    /// Mirrors the suspend counts the `SuspendThread`/`ResumeThread` shims keep
    /// in the host, so the scheduler stops entering threads whose count is
    /// nonzero. Suspension is orthogonal to parking: a suspended waiter that
    /// is woken stays put until it is resumed, and a resumed thread only runs
    /// when the scheduler reaches it.
    pub fn set_suspends(&mut self, counts: &std::collections::HashMap<u32, u32>) {
        for thread in &mut self.threads {
            thread.suspends = counts.get(&thread.id).copied().unwrap_or(0);
        }
    }

    /// Makes a parked thread runnable, with the value its wait answered.
    pub fn wake(&mut self, id: u32, answer: u32) {
        if let Some(thread) = self.thread_mut(id) {
            if thread.state == State::Waiting {
                thread.state = State::Runnable;
                thread.answer = Some(answer);
            }
        }
    }

    /// Puts a thread the CPU can run in place of one that cannot, answering where
    /// it is. `None` means no thread can run.
    ///
    /// The thread that is running keeps the CPU unless it gave it up or cannot go
    /// on, which is what makes a switch cost nothing in the common case.
    pub fn schedule(&mut self) -> Option<usize> {
        let running = &self.threads[self.current];
        if running.running() && !running.yielded {
            return Some(self.current);
        }
        let count = self.threads.len();
        for step in 1..=count {
            let index = (self.current + step) % count;
            let thread = &self.threads[index];
            // A thread that just yielded comes last: it has had its turn.
            // A suspended thread never runs, yielded or not.
            if thread.running() && !thread.yielded {
                return Some(index);
            }
        }
        // Every thread that can run has had a turn, so they all get another one.
        // A thread yields to let the others go first, not to be left out of the
        // rotation for good: two threads that yield to each other have to keep
        // meeting, or the one that yielded last would never be entered again.
        for thread in &mut self.threads {
            thread.yielded = false;
        }
        for step in 1..=count {
            let index = (self.current + step) % count;
            if self.threads[index].running() {
                return Some(index);
            }
        }
        // The one that is running is the only one left, and it gave way only to
        // let someone else in.
        self.threads[self.current].running().then_some(self.current)
    }

    /// Moves the CPU onto thread `index`, answering the registers to run it with.
    pub fn run_on(&mut self, index: usize) -> (u32, Registers, Fpu) {
        self.current = index;
        let thread = &mut self.threads[index];
        thread.state = State::Runnable;
        thread.yielded = false;
        thread.saved = false;
        (thread.id, thread.regs, thread.fpu.clone())
    }

    /// The threads that are waiting on something, for the message a hold-up
    /// prints.
    pub fn waiting_ids(&self) -> Vec<u32> {
        self.threads
            .iter()
            .filter(|thread| thread.state == State::Waiting)
            .map(|thread| thread.id)
            .collect()
    }
}

impl Default for Threads {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registers() -> Registers {
        Registers::default()
    }

    /// A machine with the thread the session starts on.
    fn started() -> Threads {
        let mut threads = Threads::new();
        threads.start(registers(), Fpu::default());
        threads
    }

    /// Runs the running thread, as the session does after choosing it.
    fn run(threads: &mut Threads) -> u32 {
        let index = threads.schedule().expect("a thread can run");
        threads.run_on(index).0
    }

    /// A thread's record is only its own while it is parked.
    ///
    /// `run_on` marks it unsaved because the registers have gone to the CPU;
    /// `park` puts them back. The session parks a thread it finds unsaved on the
    /// way out, so a switch can never leave a record holding the context the
    /// thread was spawned with -- which is how a suspended thread used to come
    /// back at its entry point instead of where it stopped.
    #[test]
    fn a_running_thread_is_unsaved_until_it_parks() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());
        assert!(threads.current_thread().saved, "a thread that has not run has nothing to lose");

        run(&mut threads);
        assert!(!threads.current_thread().saved, "a running thread's record is behind");

        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert!(threads.current_thread().saved, "parking puts the registers back");
    }

    #[test]
    fn a_parked_thread_hands_the_cpu_to_one_that_can_run() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());

        assert_eq!(run(&mut threads), 0, "the running thread keeps the CPU");
        threads.park(registers(), Fpu::default(), &ThreadWant::Wait { timeout: None });
        assert_eq!(threads.waiting_ids(), vec![0]);
        assert_eq!(run(&mut threads), 1, "the worker gets it");
    }

    #[test]
    fn a_woken_thread_comes_back_with_the_answer_its_wait_gave() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());

        // The main thread gives way, so the worker starts and waits for work.
        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1);
        threads.park(registers(), Fpu::default(), &ThreadWant::Wait { timeout: None });
        assert_eq!(threads.waiting_ids(), vec![1]);

        // The main thread runs while the worker waits, and keeps the CPU when a
        // release makes the worker runnable again: nothing takes the CPU from it
        // but its own quantum.
        assert_eq!(run(&mut threads), 0);
        threads.wake(1, 0x102);
        assert_eq!(run(&mut threads), 0);

        // It gets the CPU when the main thread gives it up, and `eax` is the
        // wait's answer rather than whatever the shim that parked it returned.
        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1);
        assert_eq!(threads.current_thread_mut().answer.take(), Some(0x102));
    }

    #[test]
    fn two_threads_that_yield_to_each_other_keep_meeting() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());

        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1, "the thread that just started goes first");
        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(
            run(&mut threads),
            0,
            "a thread that yielded is only waiting for the others, not left out"
        );
    }

    #[test]
    fn a_suspended_thread_is_never_scheduled() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());

        let mut counts = std::collections::HashMap::new();
        counts.insert(1, 1);
        threads.set_suspends(&counts);
        assert_eq!(run(&mut threads), 0);
        assert_eq!(run(&mut threads), 0, "the worker stays parked while suspended");

        // A nested suspend deepens the count; one resume is not enough.
        counts.insert(1, 2);
        threads.set_suspends(&counts);
        counts.insert(1, 1);
        threads.set_suspends(&counts);
        assert_eq!(run(&mut threads), 0);

        // Resumed to zero, the worker runs again.
        counts.insert(1, 0);
        threads.set_suspends(&counts);
        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1);
    }

    #[test]
    fn a_suspended_waiter_that_is_woken_stays_put_until_resumed() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());

        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1);
        threads.park(registers(), Fpu::default(), &ThreadWant::Wait { timeout: None });

        let mut counts = std::collections::HashMap::new();
        counts.insert(1, 1);
        threads.set_suspends(&counts);
        threads.wake(1, 0x102);
        // The wait completed, but the thread is still suspended.
        assert_eq!(run(&mut threads), 0);

        counts.insert(1, 0);
        threads.set_suspends(&counts);
        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1);
        assert_eq!(threads.current_thread_mut().answer.take(), Some(0x102));
    }

    #[test]
    fn an_exited_thread_is_never_scheduled_again() {
        let mut threads = started();
        threads.spawn(1, 7, registers(), Fpu::default());
        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 1);

        threads.park(registers(), Fpu::default(), &ThreadWant::Exit(3));
        assert!(!threads.all_exited(), "the main thread is still running");
        assert_eq!(run(&mut threads), 0);

        threads.park(registers(), Fpu::default(), &ThreadWant::Exit(0));
        assert!(threads.all_exited());
        assert!(threads.switch_wanted(), "the session has to look for another thread");
        assert_eq!(threads.schedule(), None);
    }

    #[test]
    fn a_lone_thread_that_yields_keeps_the_cpu() {
        let mut threads = started();

        threads.park(registers(), Fpu::default(), &ThreadWant::Yield);
        assert_eq!(run(&mut threads), 0);
        assert!(
            !threads.current_thread_mut().yielded,
            "a thread that has been entered again is no longer standing aside"
        );
    }
}
