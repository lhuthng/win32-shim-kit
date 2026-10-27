//! The host side of the Win32 boundary.
//!
//! The interpreter stops on every call through an import slot and hands it here.
//! A shim answers in the guest's own terms: it reads arguments from the guest
//! stack, writes results into guest memory, and returns the value the guest
//! expects in `eax`. Anything without a shim stays a
//! [`crate::session::Outcome::HostCall`], which is the diagnostic the page shows
//! and the plan requires.
//!
//! # Why the shim owns the arity
//!
//! The dispatch table only publishes an arity when two call sites agree, and
//! most slots have none. A shim, however, is written against a known signature,
//! so its [`Binding::arg_bytes`] is authoritative: the session cross-checks it
//! against the table when the table has an opinion, and otherwise uses it to pop
//! the arguments exactly as a stdcall callee would. Guessing from the table is
//! never done; a disagreement is an error, not a preference.

/// Whether a debug switch is on, looked up once and remembered.
///
/// `std::env::var` searches the environment every time it is called, which is far
/// too expensive for a path a guest reaches thousands of times a frame -- a
/// DirectDraw blit, a GDI copy, a blend. Each switch gets a cell of its own; after
/// the first call reading it is a relaxed load.
pub fn switch(cell: &'static std::sync::OnceLock<bool>, name: &'static str) -> bool {
    *cell.get_or_init(|| std::env::var(name).is_ok())
}

pub mod advapi32;
pub mod comdlg32;
pub mod crt;
pub mod ddraw;
pub mod dinput;
pub mod dsound;
pub mod files;
pub mod gdi32;
pub mod gdiplus;
pub mod kernel32;
pub mod msimg32;
pub mod msvcp;
pub mod ole32;
pub mod quartz;
pub mod sdl;
pub mod shell32;
pub mod shlwapi;
pub mod user32;
pub mod version;
pub mod winmm;
pub mod wsock32;

/// Every implemented import, one table per DLL. `binding` searches them in
/// order and the tests treat them as the whole surface.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::dispatch::Slot;
use crate::memory::{GuestMemory, HEAP_BASE, MEMORY_TOP};
use crate::Result;

/// What a shim did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// The guest continues with this value in `eax`.
    Return(u32),
    /// A 64-bit answer, which this calling convention returns in `edx:eax`.
    /// [`_time64`](crate::shim::crt) is the one import here that returns one.
    Return64(u64),
    /// The guest asked to terminate; the session stops as `Halted`.
    Exit(u32),
    /// The answer is a `double`, or an integer derived from one: the shim cannot
    /// reach the x87 stack, so it names the operation and the session applies it
    /// (see [`crate::cpu::FloatOp`]).
    Float(crate::cpu::FloatOp),
}

/// One implemented import. `arg_bytes` is the shim's own stdcall signature.
#[derive(Clone, Copy)]
pub struct Binding {
    pub dll: &'static str,
    pub name: &'static str,
    pub arg_bytes: u32,
    pub shim: Shim,
}

pub type Shim = fn(&mut Host, &mut Call<'_>) -> Result<Action>;

/// Guest stack window a shim may read its arguments from.
pub struct Call<'a> {
    pub memory: &'a mut GuestMemory,
    /// Address of the first argument (the caller's ESP before the call).
    pub args: u32,
    /// Argument bytes the shim declares.
    pub arg_bytes: u32,
}

impl Call<'_> {
    pub fn arg(&self, index: u32) -> Result<u32> {
        if (index + 1) * 4 > self.arg_bytes {
            return Err(format!(
                "argument {index} is outside the {} byte(s) this shim declares",
                self.arg_bytes
            ));
        }
        self.memory.read_u32(self.args.wrapping_add(index * 4))
    }

    pub fn cstr(&self, index: u32) -> Result<String> {
        self.memory.read_cstr(self.arg(index)?)
    }

    /// The UTF-16 string at `address`, to its terminator: what the `...W` entry
    /// points pass, where an id below `0x10000` is an integer and not a pointer.
    pub fn wstr(&self, address: u32) -> Result<String> {
        if address == 0 {
            return Ok(String::new());
        }
        // The cap matches the ANSI reader's, so a missing terminator is a stop
        // rather than a walk to the end of the arena.
        let mut units = Vec::new();
        for offset in 0..4096_u32 {
            let unit = self.memory.read_u16(address + offset * 2)?;
            if unit == 0 {
                break;
            }
            units.push(unit);
        }
        Ok(String::from_utf16_lossy(&units))
    }

    pub fn write(&mut self, va: u32, bytes: &[u8]) -> Result<()> {
        // A zero-length copy touches no address, which is why Win32 accepts a
        // null buffer with a zero count. Rejecting it made an end-of-file read
        // look like a fault at address zero.
        if bytes.is_empty() {
            return Ok(());
        }
        self.memory.write_bytes(va, bytes)
    }

    pub fn write_u32(&mut self, va: u32, value: u32) -> Result<()> {
        self.memory.write_u32(va, value)
    }

    pub fn write_u16(&mut self, va: u32, value: u16) -> Result<()> {
        self.memory.write_u16(va, value)
    }

    pub fn write_cstr(&mut self, va: u32, text: &str) -> Result<()> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        self.write(va, &bytes)
    }

    /// Zeroes `len` bytes, which is what the Win32 structures need.
    pub fn zero(&mut self, va: u32, len: u32) -> Result<()> {
        const ZEROS: [u8; 256] = [0; 256];
        let mut at = va;
        let mut left = len;
        while left > 0 {
            let chunk = left.min(ZEROS.len() as u32);
            self.memory.write_bytes(at, &ZEROS[..chunk as usize])?;
            at = at.wrapping_add(chunk);
            left -= chunk;
        }
        Ok(())
    }
}

/// A guest routine a shim wants run before its import returns.
///
/// A shim cannot call guest code itself: it runs inside the interpreter, and the
/// guest's frame is suspended underneath. So it leaves the call here and returns
/// normally; the session pushes the arguments, sets the program counter, and
/// finishes the import with the routine's return value when the trampoline is
/// reached. See [`crate::session::Session::run`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GuestCall {
    pub entry: u32,
    /// Arguments, left to right, pushed by the session.
    pub args: [u32; 4],
    /// What the import returns, when that is not the routine's own value.
    ///
    /// `LoadLibraryA` runs the library's `DllMain` and then has to answer with
    /// the module handle, which `DllMain` knows nothing about.
    pub answer: Option<u32>,
    /// Whether the shim wants to run again when the routine returns.
    ///
    /// A table walk (`_initterm`) calls one entry per pass and needs the next, and
    /// `exit` runs its handlers one at a time. The session re-enters the shim with
    /// the import's own frame restored; a resumed shim reads its state from
    /// [`Host`], never from its arguments, because the routine's frame was pushed
    /// over them.
    pub resume: bool,
}

/// What a `DirectInput` device is, which decides what `GetDeviceState` writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Keyboard,
    Mouse,
    Joystick,
}

/// Host state the guest can observe: the last error, the exit code, and the
/// arena every allocation comes from.
pub struct Host {
    /// The program this machine is running. Carried here so a shim can ask the
    /// target for its own name and screen without a port's facts leaking into
    /// the shim.
    pub target: &'static crate::target::Target,
    pub last_error: u32,
    pub exit_code: Option<u32>,
    /// Next never-used address in the heap arena. Memory below it that the guest
    /// released is reusable; this only moves when no hole can serve a request.
    arena: u32,
    allocations: HashMap<u32, u32>,
    /// Free blocks, by size and then address.
    ///
    /// A board session creates and destroys a surface per tile as it loads, and
    /// the game's CRT heap churns on every frame, so a freed block has to be
    /// reusable. The previous version scanned a list of every block ever freed,
    /// which made each allocation cost more the longer the session ran -- the
    /// per-frame cost that made movement crawl -- and never merged neighbours,
    /// so the bump pointer walked to the end of the arena and the board stopped.
    /// Size-ordered lookup is `O(log n)` per allocation, whatever the session has
    /// done so far.
    free_by_size: BTreeMap<u32, BTreeSet<u32>>,
    /// The same holes by address, so a release can merge with the block before it
    /// and the one after it: a hole is what a release leaves, not what a guest
    /// once allocated.
    free_by_address: BTreeMap<u32, u32>,
    /// Bytes available in the holes, kept as a running total rather than summed
    /// on demand.
    free_bytes: u64,
    next_heap: u32,
    command_line: Option<u32>,
    environment: Option<u32>,
    environment_wide: Option<u32>,
    /// Window classes, windows and pointer state (`user32`).
    pub windows: user32::Windows,
    /// The registry tree (`advapi32`), in memory for the life of the session.
    pub registry: advapi32::Registry,
    /// The bytes the guest may read (`files`, and the file half of `kernel32`).
    pub files: files::Files,
    /// The display: surfaces, the palette and the flip counter (`ddraw`).
    pub ddraw: ddraw::DirectDraw,
    /// The audio device, its buffers and the events the page's mixer consumes.
    pub audio: dsound::DirectSound,
    /// The SDL 1.2 surface `SDL.dll` games draw into, and the state the
    /// functions around it keep (`sdl`).
    pub sdl: sdl::Sdl,
    /// Thread-local slots `TlsAlloc` hands out. This machine has one thread, so
    /// the slots are session state rather than a real per-thread map.
    pub tls: kernel32::ThreadLocals,
    /// Multimedia timers the guest set, which the session calls back into
    /// (`winmm`).
    pub timers: winmm::Timers,
    /// Files open through `mmioOpenA`, with where each one is being read from.
    pub mmio: winmm::Mmio,
    /// The guest's clock in milliseconds, when the embedder supplies one. The page
    /// sets it before every chunk, which is what makes a timer follow real time
    /// instead of how fast the interpreter happens to run.
    clock_ms: Option<u64>,
    /// One guest-memory vtable per interface kind, built on first use by
    /// [`crate::thunks`] and shared by every object of that kind.
    pub vtables: [u32; crate::thunks::KIND_LIMIT as usize],
    /// Things the guest told the user rather than the host: message-box text and
    /// the windows it created. The page and the trace print these; a dialog the
    /// guest expects to block does not block yet, so this list is where its text
    /// survives.
    pub notices: Vec<String>,
    /// The keyboard, the pointer and the message queue they feed.
    pub input: crate::input::Input,
    /// What each `DirectInput` device object is, keyed by the object address the
    /// guest was handed. The object itself is four bytes of vtable pointer.
    pub devices: HashMap<u32, DeviceKind>,
    /// A guest routine the shim that just ran wants called before its import
    /// returns. The session takes it; a shim never leaves this set.
    pub guest_call: Option<GuestCall>,
    /// How many times the guest polled `GetDeviceState`, which is the evidence
    /// that input is read by polling rather than by messages. `GetDeviceData` is
    /// counted separately because an empty buffer there would mean the guest is
    /// using buffered input, which this host does not deliver.
    pub device_state_reads: u64,
    pub device_data_reads: u64,
    /// Resources the image carries, in the order its tree lists them. The handle
    /// `FindResourceA` hands out is an index into this, so `LoadResource` and
    /// `SizeofResource` need nothing else.
    pub resources: Vec<crate::pe::Resource>,
    /// The bitmaps, fonts and device contexts `gdi32` hands out.
    pub gdi: gdi32::Gdi,
    /// The images `gdiplus` decoded, and the locks open on them.
    pub gdiplus: gdiplus::Gdiplus,
    /// The `DirectShow` filter graph, when the guest created one (`quartz`).
    pub quartz: quartz::Quartz,
    /// The block `GetFileVersionInfoA` filled and `VerQueryValueA` reads: where it
    /// is and how long it is (`version`).
    pub version: Option<(u32, u32)>,
    /// `argc`/`argv`, the `atexit` chain and the table walk `_initterm` is in the
    /// middle of (`crt`).
    pub crt: crt::Crt,
    /// Whether the session is re-running the shim that asked for the guest routine
    /// which just returned. A shim that asked to be resumed reads this to tell a
    /// continuation from a fresh call by the same import.
    pub resumed: bool,
    /// What the last guest routine a shim asked for left in `eax`. A walk that
    /// stops on an initialiser's failure reads it.
    pub guest_result: u32,
    /// The `ecx` the guest called the import through, which is where MSVC passes
    /// `this` for a `__thiscall` member function. A C++ runtime shim reads it from
    /// here; every other shim ignores it. The session sets it before running a
    /// shim, so it describes the call in flight.
    pub this: u32,
    /// MSVC6's C++ runtime state: the shared empty string and the `npos` value
    /// (`msvcp`).
    pub msvcp: msvcp::Strings,
    /// The bindings `GetProcAddress` has handed out, in the order it resolved
    /// them: entry `n` lives at `PROC_BASE + n * 4`.
    pub procs: Vec<&'static Binding>,
    /// The module names `GetModuleHandleA` and `LoadLibraryA` have answered for,
    /// so a handle stays the same number across calls. The handle is an index into
    /// this, above `kernel32`'s module base.
    pub modules: Vec<String>,
}

impl Default for Host {
    fn default() -> Self {
        Self::new()
    }
}

impl Host {
    /// A host with no target; the unit tests in this crate.
    pub fn new() -> Self {
        Self::with_target(&crate::target::DEFAULT)
    }

    pub fn with_target(target: &'static crate::target::Target) -> Self {
        let (width, height, depth) = target.screen;
        Self {
            target,
            last_error: 0,
            exit_code: None,
            arena: HEAP_BASE,
            allocations: HashMap::new(),
            free_by_size: BTreeMap::new(),
            free_by_address: BTreeMap::new(),
            free_bytes: 0,
            next_heap: 1,
            command_line: None,
            environment: None,
            environment_wide: None,
            windows: user32::Windows::with_screen(i32::from(width), i32::from(height)),
            registry: advapi32::Registry::default(),
            files: files::Files::default(),
            ddraw: ddraw::DirectDraw::with_screen(u32::from(width), u32::from(height), depth),
            audio: dsound::DirectSound::default(),
            sdl: sdl::Sdl::default(),
            tls: kernel32::ThreadLocals::default(),
            timers: winmm::Timers::default(),
            mmio: winmm::Mmio::default(),
            clock_ms: None,
            vtables: [0; crate::thunks::KIND_LIMIT as usize],
            notices: Vec::new(),
            input: crate::input::Input::new(),
            devices: HashMap::new(),
            guest_call: None,
            device_state_reads: 0,
            device_data_reads: 0,
            resources: Vec::new(),
            gdi: gdi32::Gdi::default(),
            gdiplus: gdiplus::Gdiplus::default(),
            quartz: quartz::Quartz::default(),
            version: None,
            crt: crt::Crt::default(),
            resumed: false,
            guest_result: 0,
            this: 0,
            msvcp: msvcp::Strings::default(),
            procs: Vec::new(),
            modules: Vec::new(),
        }
    }

    /// Sets the guest's clock, in milliseconds. Monotonic, and only ever read as
    /// a difference, so any origin works: the page passes `performance.now()`.
    ///
    /// It is not moved back: `GetTickCount` that goes backwards makes every "is
    /// the next frame due yet" test wrong. Nothing here ever moves it forward
    /// either, which is the point: the only clock a guest sees is the embedder's,
    /// so a game that waits for its own next frame waits for real time. Moving
    /// it forward on a `Sleep` was how this host used to invent time, and a game
    /// then ran at whatever rate the interpreter could manage -- Cave Story's
    /// clock measured 44x real time that way.
    pub fn set_clock(&mut self, milliseconds: u64) {
        self.clock_ms = Some(milliseconds.max(self.clock_ms.unwrap_or(0)));
    }

    /// The guest's time in milliseconds: the embedder's clock when it supplied
    /// one, otherwise the instruction count at the declared exchange rate, which
    /// is what a headless run without a clock falls back to.
    pub fn guest_time_ms(&self, instructions: u64) -> u64 {
        self.clock_ms
            .unwrap_or(instructions / winmm::INSTRUCTIONS_PER_MILLISECOND)
    }

    /// The page's clock, for state that has to follow real time between host
    /// calls (`DirectSound` playheads) rather than the instruction count.
    pub fn now_ms(&self) -> u64 {
        self.clock_ms.unwrap_or(0)
    }

    /// Records a line the user would otherwise have seen in a window.
    pub fn notice(&mut self, text: String) {
        self.notices.push(text);
    }

    /// Allocates out of the heap arena. `VirtualAlloc` gets the same memory, so
    /// the two are interchangeable here.
    ///
    /// The smallest hole that can hold the request is used first, and a block
    /// that is larger than the request is split, with both pieces -- the
    /// alignment padding before the block and the space after it -- going back to
    /// the free list. Memory is not zeroed here: a fresh page is already zero
    /// because the arena starts zeroed, and a reused block holds what its last
    /// owner left, exactly as `HeapAlloc` does.
    pub fn allocate(&mut self, size: u32, align: u32) -> Result<u32> {
        let size = size.max(1);
        let align = align.max(4).next_power_of_two();
        if let Some((start, length, aligned)) = self.best_fit(size, align) {
            self.free_remove(start, length);
            // The padding between the hole and the aligned address stays free.
            if aligned > start {
                self.free_insert(start, aligned - start);
            }
            let tail = length - (aligned - start) - size;
            if tail > 0 {
                self.free_insert(aligned + size, tail);
            }
            self.allocations.insert(aligned, size);
            return Ok(aligned);
        }
        let start = (self.arena + (align - 1)) & !(align - 1);
        let end = start
            .checked_add(size)
            .ok_or_else(|| format!("cannot allocate {size} bytes: address space wraps"))?;
        if end > MEMORY_TOP {
            return Err(format!(
                "cannot allocate {size} bytes at {start:#010x}: the arena ends at {MEMORY_TOP:#010x} \
                 ({} bytes are free in {} hole(s))",
                self.free_bytes,
                self.free_by_address.len()
            ));
        }
        self.arena = end;
        self.allocations.insert(start, size);
        Ok(start)
    }

    /// The smallest free block that can hold `size` at `align`, as
    /// `(block start, block length, aligned start)`.
    fn best_fit(&self, size: u32, align: u32) -> Option<(u32, u32, u32)> {
        for (length, starts) in self.free_by_size.range(size..) {
            for start in starts {
                let aligned = start + (start.wrapping_neg() & (align - 1));
                if *length >= size + (aligned - start) {
                    return Some((*start, *length, aligned));
                }
            }
        }
        None
    }

    /// Records a hole, merging it with the free blocks that touch it so two
    /// neighbours become one block a larger request can use.
    fn free_insert(&mut self, address: u32, size: u32) {
        if size == 0 {
            return;
        }
        let mut start = address;
        let mut length = size;
        // The block that ends exactly where this one starts.
        if let Some((&before, &before_length)) = self.free_by_address.range(..start).next_back() {
            if before + before_length == start {
                self.free_remove(before, before_length);
                start = before;
                length += before_length;
            }
        }
        // And the block that starts exactly where this one ends.
        if let Some(&after_length) = self.free_by_address.get(&(start + length)) {
            self.free_remove(start + length, after_length);
            length += after_length;
        }
        self.free_by_size.entry(length).or_default().insert(start);
        self.free_by_address.insert(start, length);
        self.free_bytes += u64::from(length);
    }

    fn free_remove(&mut self, address: u32, size: u32) {
        if let Some(starts) = self.free_by_size.get_mut(&size) {
            starts.remove(&address);
            if starts.is_empty() {
                self.free_by_size.remove(&size);
            }
        }
        self.free_by_address.remove(&address);
        self.free_bytes = self.free_bytes.saturating_sub(u64::from(size));
    }

    /// Cuts `[address, address + size)` out of any hole that contains it, for a
    /// `VirtualAlloc` at a fixed address that lands in reusable memory.
    fn free_carve(&mut self, address: u32, size: u32) {
        let Some((&start, &length)) = self.free_by_address.range(..=address).next_back() else {
            return;
        };
        let end = address.saturating_add(size);
        if start.saturating_add(length) < end {
            return;
        }
        self.free_remove(start, length);
        if address > start {
            self.free_insert(start, address - start);
        }
        let tail = start.saturating_add(length) - end;
        if tail > 0 {
            self.free_insert(end, tail);
        }
    }

    /// Hands a block back so a later allocation can reuse it. Nothing is
    /// unmapped: the guest address stays valid, which is what a released
    /// DirectDraw surface's pixels would be on Windows too.
    pub fn release(&mut self, address: u32) {
        if let Some(size) = self.allocations.remove(&address) {
            self.free_insert(address, size);
        }
    }

    /// Honours an explicit `VirtualAlloc` address. Committing a region that is
    /// already allocated is idempotent, which is what the CRT does after a
    /// reserve: it commits page ranges of memory it already owns.
    pub fn allocate_at(&mut self, address: u32, size: u32) -> Result<u32> {
        let size = size.max(1);
        let end = address
            .checked_add(size)
            .ok_or_else(|| format!("VirtualAlloc at {address:#010x} wraps the address space"))?;
        if end > MEMORY_TOP {
            return Err(format!(
                "VirtualAlloc at {address:#010x} for {size} bytes ends at {end:#010x}, past {MEMORY_TOP:#010x}"
            ));
        }

        for (start, length) in &self.allocations {
            let allocated_end = start.saturating_add(*length);
            if address < allocated_end && *start < end {
                if address >= *start && end <= allocated_end {
                    return Ok(address);
                }
                return Err(format!(
                    "VirtualAlloc at {address:#010x} for {size} bytes overlaps the allocation at {start:#010x}"
                ));
            }
        }

        self.free_carve(address, size);
        self.allocations.insert(address, size);
        self.arena = self.arena.max(end);
        Ok(address)
    }

    pub fn reallocate(
        &mut self,
        memory: &mut GuestMemory,
        pointer: u32,
        size: u32,
        align: u32,
    ) -> Result<u32> {
        let previous = self.allocations.get(&pointer).copied().unwrap_or(0);
        let moved = self.allocate(size, align)?;
        let copy = previous.min(size);
        if copy > 0 {
            let bytes = memory.read_bytes(pointer, copy as usize)?.to_vec();
            memory.write_bytes(moved, &bytes)?;
        }
        if previous > 0 {
            self.release(pointer);
        }
        Ok(moved)
    }

    pub fn allocated(&self, pointer: u32) -> Option<u32> {
        self.allocations.get(&pointer).copied()
    }

    /// How much of the heap arena is in use, so a run can report whether it is
    /// close to the ceiling instead of failing mid-board.
    pub fn heap_used(&self) -> u32 {
        self.arena.saturating_sub(HEAP_BASE)
    }

    /// Bytes handed back but not yet reused, which is what fragmentation costs.
    pub fn heap_free(&self) -> u32 {
        self.free_bytes.min(u64::from(u32::MAX)) as u32
    }

    /// How many separate holes the free list holds, which is what says whether a
    /// session is fragmenting or reusing.
    pub fn heap_holes(&self) -> u32 {
        self.free_by_address.len() as u32
    }

    pub fn allocation_count(&self) -> u32 {
        self.allocations.len() as u32
    }

    pub fn surface_count(&self) -> u32 {
        self.ddraw.surface_count() as u32
    }

    pub fn new_heap(&mut self) -> u32 {
        let handle = self.next_heap;
        self.next_heap += 1;
        handle
    }

    // ---- input ----------------------------------------------------------

    /// The window the page's input belongs to: the guest's own window, because
    /// this host has no other.
    pub fn input_window(&self) -> u32 {
        self.windows.main_window()
    }

    /// A key changed state. Both views are updated from here, so DirectInput and
    /// the message queue can never disagree about what is held.
    pub fn key(&mut self, dik: u8, down: bool) {
        let hwnd = self.input_window();
        self.input.key(dik, down, hwnd);
    }

    /// The pointer moved to a client position.
    ///
    /// A DirectInput mouse reports movement, not a position, so the step the
    /// guest gets is measured from the cursor the host is tracking -- what
    /// `GetCursorPos` answers -- rather than from wherever the page's pointer was
    /// last seen. A page that drives a game drawing its own cursor sends
    /// movement instead, through [`Host::pointer_move`]; this is for a caller
    /// that knows the position, such as a test or a shell.
    pub fn pointer(&mut self, x: i32, y: i32) {
        let hwnd = self.input_window();
        let (cx, cy) = self.windows.cursor();
        self.windows.set_cursor(x, y);
        self.input.move_by(x.saturating_sub(cx), y.saturating_sub(cy), x, y, hwnd);
    }

    /// The pointer moved by a delta, as a locked pointer reports it.
    ///
    /// The movement is delivered whole; only the position a message carries is
    /// held against the display, so a move at the edge still reaches the guest
    /// instead of being flattened to nothing.
    ///
    /// The base is the cursor the guest sees: `GetCursorPos`, which the guest's
    /// own `SetCursorPos` moves. The game reads the cursor by position and parks
    /// it in the middle after each read, so the movement has to be added to that
    /// parked point. Adding it to the page's last position instead drifts the
    /// reported point to the window edge, where it stops changing -- a fast
    /// flick then reads as no movement at all, while a slow one still lands.
    pub fn pointer_move(&mut self, dx: i32, dy: i32) {
        let hwnd = self.input_window();
        let (width, height) = (self.ddraw.width() as i32, self.ddraw.height() as i32);
        let (x, y) = self.windows.cursor();
        let hold = |value: i32, limit: i32| {
            if limit > 0 {
                value.clamp(0, limit - 1)
            } else {
                value
            }
        };
        let (x, y) = (
            hold(x.saturating_add(dx), width),
            hold(y.saturating_add(dy), height),
        );
        self.windows.set_cursor(x, y);
        self.input.move_by(dx, dy, x, y, hwnd);
    }

    /// A pointer button changed state.
    pub fn pointer_button(&mut self, button: u8, down: bool) {
        let hwnd = self.input_window();
        self.input.button(button, down, hwnd);
    }

    pub fn command_line(&mut self, memory: &mut GuestMemory) -> Result<u32> {
        if let Some(pointer) = self.command_line {
            return Ok(pointer);
        }
        let text = format!("\"{}\"", self.target.exe_name);
        let pointer = self.allocate(text.len() as u32 + 1, 4)?;
        memory.write_bytes(pointer, text.as_bytes())?;
        memory.write_bytes(pointer + text.len() as u32, &[0])?;
        self.command_line = Some(pointer);
        Ok(pointer)
    }

    pub fn environment(&mut self, memory: &mut GuestMemory) -> Result<u32> {
        if let Some(pointer) = self.environment {
            return Ok(pointer);
        }
        let pointer = self.allocate(4, 4)?;
        memory.write_bytes(pointer, &[0, 0, 0, 0])?;
        self.environment = Some(pointer);
        Ok(pointer)
    }

    pub fn environment_wide(&mut self, memory: &mut GuestMemory) -> Result<u32> {
        if let Some(pointer) = self.environment_wide {
            return Ok(pointer);
        }
        let pointer = self.allocate(4, 4)?;
        memory.write_bytes(pointer, &[0, 0, 0, 0])?;
        self.environment_wide = Some(pointer);
        Ok(pointer)
    }
}

/// Every table of implemented imports.
pub const TABLES: &[&[Binding]] = &[
    crt::SHIMS,
    msvcp::SHIMS,
    kernel32::SHIMS,
    files::SHIMS,
    comdlg32::SHIMS,
    shell32::SHIMS,
    user32::SHIMS,
    gdi32::SHIMS,
    gdiplus::SHIMS,
    msimg32::SHIMS,
    advapi32::SHIMS,
    dsound::SHIMS,
    ddraw::SHIMS,
    dinput::SHIMS,
    winmm::SHIMS,
    ole32::SHIMS,
    shlwapi::SHIMS,
    version::SHIMS,
    sdl::SHIMS,
    wsock32::SHIMS,
];

/// Whether a DLL's exports use the cdecl convention, where the **caller** pops
/// the arguments.
///
/// Win32's own DLLs are stdcall throughout; SDL 1.2 is the exception this kit
/// runs into, because its headers declare `SDLCALL` as `__cdecl`, and
/// `wsprintfA` is declared `WINAPIV` (cdecl) because it is variadic. A cdecl
/// shim's declared bytes are still its read window -- it reads the same
/// arguments -- but the import must not pop them, or the guest's own `add esp`
/// would take another copy and its stack would drift by four bytes per call.
pub fn is_cdecl(dll: &str, name: &str) -> bool {
    // Every entry point the C runtime exports is `__cdecl`; its ancestors and
    // descendants share the convention.
    dll.eq_ignore_ascii_case("SDL.dll")
        || name.eq_ignore_ascii_case("wsprintfA")
        || is_c_runtime(dll)
        // The C++ runtime's member functions are `__thiscall`, but the operators
        // declared at namespace scope -- `std::operator+`, `operator==` and the
        // rest -- are free functions and `__cdecl`. The mangling says which: a
        // global function's name carries `@@YA`.
        || (dll.eq_ignore_ascii_case(msvcp::MSVCP60) && name.contains("@@YA"))
}

/// Whether a DLL name is a C runtime in the `msvcrt` lineage: `msvcrt` itself,
/// and the `msvcr60`-style private copies a Visual C++ program ships beside its
/// executable.
///
/// They are one table here because the entry points are the same in all of them:
/// a game built with VC6 calls `_initterm` and `malloc` with the signatures VC8's
/// runtime has.
pub fn is_c_runtime(dll: &str) -> bool {
    // Case-insensitively, and without a copy: this runs for every host call a
    // guest makes, so an allocation here is a heap round trip per import.
    let stem = match dll.len().checked_sub(4) {
        Some(cut) if dll.get(cut..).is_some_and(|end| end.eq_ignore_ascii_case(".dll")) => {
            &dll[..cut]
        }
        _ => dll,
    };
    if stem.eq_ignore_ascii_case("msvcrt") {
        return true;
    }
    match strip_prefix_ignoring_case(stem, "msvcr") {
        Some(version) => !version.is_empty() && version.bytes().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// `text.strip_prefix(prefix)`, matching ASCII case the way a file name does.
fn strip_prefix_ignoring_case<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    let head = text.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix).then(|| &text[prefix.len()..])
}

/// The binding for a dispatch slot, matched on DLL and function name.
///
/// `tables` is the target's resolution order: the generic tables first, the
/// port's own after them, and **later wins**, so a port overrides one function
/// by registering it again instead of forking a table.
/// The binding filed for `name`, optionally restricted to one DLL.
///
/// This is the lookup a program does for itself with `GetProcAddress`: unlike
/// [`binding`], there is no import slot to start from, only a name -- and the DLL
/// it is asked of may be one this host answers without ever loading it.
pub fn named(
    tables: &'static [&'static [Binding]],
    dll: Option<&str>,
    name: &str,
) -> Option<&'static Binding> {
    tables
        .iter()
        .rev()
        .flat_map(|table| table.iter())
        .find(|binding| {
            (binding.name == name
                || export_name(dll.unwrap_or(binding.dll), &binding.name) == name)
                && dll.is_none_or(|dll| same_dll(binding.dll, dll))
        })
}

/// Whether this host answers for a DLL of this name at all, which is what
/// `LoadLibraryA` and `GetModuleHandleA` have to say before a program may resolve
/// anything from it.
pub fn module_known(tables: &'static [&'static [Binding]], dll: &str) -> bool {
    tables
        .iter()
        .flat_map(|table| table.iter())
        .any(|binding| same_dll(binding.dll, dll))
}

pub fn binding(tables: &'static [&'static [Binding]], slot: &Slot) -> Option<&'static Binding> {
    tables
        .iter()
        .rev()
        .flat_map(|table| table.iter())
        .find(|binding| {
            same_dll(binding.dll, &slot.dll)
                && (binding.name == slot.name
                    || binding.name == export_name(&slot.dll, &slot.name)
                    // The other direction: a binding filed by ordinal answers an
                    // image that imported the name. Cave Story imports
                    // `DirectSoundCreate` by name; the shim is filed as `#1`.
                    || slot.name == export_name(&slot.dll, binding.name))
        })
}

/// Whether a binding filed under one DLL name answers an import of another.
///
/// Names match case-insensitively, and a C runtime answers for every name in its
/// family: `MSVCRT.dll!malloc` is the same import as `msvcr80.dll!malloc`.
fn same_dll(filed: &str, imported: &str) -> bool {
    filed.eq_ignore_ascii_case(imported) || (is_c_runtime(filed) && is_c_runtime(imported))
}

/// The name a binding is filed under, for a slot that may have been imported by
/// ordinal.
///
/// An import table can name a function by number instead of by name, and the
/// engine keeps what the image wrote. Two DLLs in this kit's targets are known
/// to do it: `DSOUND.dll` exports exactly one function, `DirectSoundCreate`,
/// which older linkers import as ordinal 1, and `DDRAW.dll`'s
/// `DirectDrawCreate` is ordinal 1 as well.
fn export_name<'a>(dll: &str, name: &'a str) -> &'a str {
    if name != "#1" {
        return name;
    }
    if dll.eq_ignore_ascii_case("DSOUND.dll") {
        return "DirectSoundCreate";
    }
    if dll.eq_ignore_ascii_case("DDRAW.dll") {
        return "DirectDrawCreate";
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cpu::HostCall;
    use crate::pe::IMAGE_BASE;

    /// Signature agreement against a real import surface is a property of the
    /// executable, so it lives with the table in the port that owns it. What can
    /// be checked here is that the tables are internally consistent.
    #[test]
    fn the_socket_ordinals_an_image_imports_are_answered() {
        // NTSD Community imports WinSock by ordinal, and `#101` is the one that
        // used to stop the run when its online menu was opened.
        for ordinal in ["#101", "#115", "#116", "#23", "#2", "#4", "#19"] {
            assert!(
                named(TABLES, Some("WSOCK32.dll"), ordinal).is_some(),
                "WSOCK32.dll!{ordinal} is not answered"
            );
        }
    }

    #[test]
    fn every_table_entry_is_dword_aligned() {
        for binding in TABLES.iter().flat_map(|table| table.iter()) {
            assert!(!binding.name.is_empty());
            assert_eq!(binding.arg_bytes % 4, 0, "{} is not dword aligned", binding.name);
        }
    }

    #[test]
    fn a_later_table_overrides_an_earlier_one() {
        fn generic(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
            Ok(Action::Return(1))
        }
        fn specific(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
            Ok(Action::Return(2))
        }
        static GENERIC: &[Binding] = &[Binding {
            dll: "KERNEL32.dll",
            name: "GetVersion",
            arg_bytes: 0,
            shim: generic,
        }];
        static PORT: &[Binding] = &[Binding {
            dll: "KERNEL32.dll",
            name: "GetVersion",
            arg_bytes: 0,
            shim: specific,
        }];
        static TABLES: &[&[Binding]] = &[GENERIC, PORT];
        static SLOT: Slot = Slot {
            dll: "KERNEL32.dll",
            name: "GetVersion",
            iat_va: 0x0040_1000,
            args: None,
            sites: 1,
            agreement: 0,
            ambiguous: 0,
        };
        let found = binding(TABLES, &SLOT).expect("the slot resolves");
        assert_eq!((found.shim)(&mut Host::new(), &mut Call {
            memory: &mut GuestMemory::empty(0x1000).unwrap(),
            args: 0,
            arg_bytes: 0,
        }).unwrap(), Action::Return(2));
    }

    /// A program that ships its own C runtime names it `msvcr80.dll` or
    /// `msvcr100.dll`; the entry points are the ones `msvcrt.dll` has, so the one
    /// table has to answer for every name in the family.
    #[test]
    fn one_table_answers_for_every_c_runtime_name() {
        for name in ["MSVCRT.dll", "msvcrt.dll", "MSVCR60.dll", "msvcr71.dll", "MSVCR80.dll", "msvcr100.dll"] {
            let slot = Slot {
                dll: Box::leak(name.to_string().into_boxed_str()),
                name: "malloc",
                iat_va: 0x0040_1000,
                args: Some(4),
                sites: 2,
                agreement: 2,
                ambiguous: 0,
            };
            let found = binding(TABLES, &slot).unwrap_or_else(|| panic!("malloc is missing from {name}"));
            assert_eq!(found.name, "malloc");
        }
        // A name that merely looks similar is not the runtime.
        assert!(!is_c_runtime("msvcrt-old.dll"));
        assert!(!is_c_runtime("msvcp60.dll"));
    }

    /// The C runtime exports `__cdecl` functions, so their import must leave the
    /// arguments for the caller's own `add esp`.
    #[test]
    fn every_c_runtime_import_is_cdecl() {
        for binding in TABLES.iter().flat_map(|table| table.iter()) {
            if is_c_runtime(binding.dll) {
                assert!(is_cdecl(binding.dll, binding.name), "{}!{}", binding.dll, binding.name);
            }
        }
    }

    /// Two tables must not claim the same slot: the first match wins, and a
    /// duplicate would silently shadow the other.
    #[test]
    fn no_slot_is_implemented_twice() {
        let mut seen = Vec::new();
        for binding in TABLES.iter().flat_map(|table| table.iter()) {
            let key = format!("{}!{}", binding.dll.to_ascii_lowercase(), binding.name);
            assert!(!seen.contains(&key), "{key} is bound twice");
            seen.push(key);
        }
    }



    #[test]
    fn allocation_is_aligned_and_zeroed() {
        let memory = GuestMemory::empty(crate::memory::HEAP_BASE as usize + 0x1000)
            .expect("blank memory");
        let mut host = Host::new();
        let first = host.allocate(10, 4).unwrap();
        let second = host.allocate(8, 16).unwrap();
        assert_eq!(first % 4, 0);
        assert_eq!(second % 16, 0);
        assert!(second >= first + 10);
        assert_eq!(memory.read_bytes(first, 10).unwrap(), &[0_u8; 10]);
        assert_eq!(host.allocated(first), Some(10));
    }

    #[test]
    fn a_released_block_is_reused_instead_of_growing_the_arena() {
        let mut host = Host::new();
        let first = host.allocate(4096, 4).unwrap();
        let used = host.heap_used();
        host.release(first);
        assert_eq!(host.heap_free(), 4096);
        let again = host.allocate(4096, 4).unwrap();
        assert_eq!(again, first, "the hole is the same size, so it fits exactly");
        assert_eq!(host.heap_used(), used, "no new arena was needed");
        assert_eq!(host.heap_free(), 0);
    }

    #[test]
    fn neighbouring_holes_merge_into_one_a_larger_request_fits() {
        let mut host = Host::new();
        let base = host.allocate(1024, 4).unwrap();
        let second = host.allocate(1024, 4).unwrap();
        assert_eq!(second, base + 1024);
        let used = host.heap_used();

        // Freed in the order that leaves two adjacent holes, not one.
        host.release(base);
        host.release(second);
        assert_eq!(host.heap_free(), 2048, "two holes, 2 KiB together");

        // A 2 KiB request is served by the merged hole rather than by the tail.
        let joined = host.allocate(2048, 4).unwrap();
        assert_eq!(joined, base);
        assert_eq!(host.heap_used(), used, "nothing new came out of the arena");
    }

    #[test]
    fn a_hole_serves_an_aligned_request_by_keeping_the_padding_free() {
        let mut host = Host::new();
        let block = host.allocate(64, 4).unwrap();
        host.release(block);
        // Asking for 16-byte alignment inside a 4-aligned hole: the padding before
        // the aligned address is a hole of its own, not lost.
        let aligned = host.allocate(16, 16).unwrap();
        assert_eq!(aligned % 16, 0);
        assert!(aligned >= block);
        let padding = aligned - block;
        assert_eq!(host.heap_free(), 64 - padding - 16, "the padding is still free");
    }

    #[test]
    fn a_virtual_alloc_into_a_hole_carves_it_out() {
        let mut host = Host::new();
        let block = host.allocate(0x3000, 4).unwrap();
        host.release(block);
        assert_eq!(host.heap_free(), 0x3000);

        let inside = block + 0x1000;
        assert_eq!(host.allocate_at(inside, 0x1000).unwrap(), inside);
        assert_eq!(
            host.heap_free(),
            0x2000,
            "the head and tail of the hole stay available"
        );
        assert_eq!(host.allocated(inside), Some(0x1000));
        // And the two halves are still usable.
        assert_eq!(host.allocate(0x1000, 4).unwrap(), block);
        assert_eq!(host.allocate(0x1000, 4).unwrap(), inside + 0x1000);
    }

    #[test]
    fn arguments_are_read_from_the_guest_stack() {
        let mut memory = GuestMemory::empty(0x1000).unwrap();
        let args = IMAGE_BASE + 0x800;
        memory
            .write_bytes(args, &[1, 0, 0, 0, 2, 0, 0, 0])
            .unwrap();
        let call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 8,
        };
        assert_eq!(call.arg(0).unwrap(), 1);
        assert_eq!(call.arg(1).unwrap(), 2);
        assert!(call.arg(2).is_err(), "reading past the declared signature");
    }

    #[test]
    fn a_host_call_reports_the_first_argument_address() {
        let call = HostCall {
            slot: 0,
            thunk: None,
            proc: None,
            iat_va: 0,
            return_address: 0,
            args: 0x00ff_ff00,
            esp: 0x00ff_ff08,
            arg_bytes: Some(8),
        };
        // The arguments sit at the caller's ESP, before the call pushed its
        // return address; the shim reads arg 0 there.
        assert_eq!(call.args, 0x00ff_ff00);
        assert_eq!(call.args.wrapping_add(4), 0x00ff_ff04);
    }
}
