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

pub mod advapi32;
pub mod ddraw;
pub mod dinput;
pub mod dsound;
pub mod files;
pub mod gdi32;
pub mod kernel32;
pub mod user32;
pub mod winmm;

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
    /// The guest asked to terminate; the session stops as `Halted`.
    Exit(u32),
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

    pub fn write(&mut self, va: u32, bytes: &[u8]) -> Result<()> {
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
    /// Multimedia timers the guest set, which the session calls back into
    /// (`winmm`).
    pub timers: winmm::Timers,
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
            timers: winmm::Timers::default(),
            clock_ms: None,
            vtables: [0; crate::thunks::KIND_LIMIT as usize],
            notices: Vec::new(),
            input: crate::input::Input::new(),
            devices: HashMap::new(),
            guest_call: None,
            device_state_reads: 0,
            device_data_reads: 0,
        }
    }

    /// Sets the guest's clock, in milliseconds. Monotonic, and only ever read as
    /// a difference, so any origin works: the page passes `performance.now()`.
    pub fn set_clock(&mut self, milliseconds: u64) {
        self.clock_ms = Some(milliseconds);
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
    pub fn pointer(&mut self, x: i32, y: i32) {
        let hwnd = self.input_window();
        self.windows.set_cursor(x, y);
        self.input.pointer(x, y, hwnd);
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
    kernel32::SHIMS,
    files::SHIMS,
    user32::SHIMS,
    gdi32::SHIMS,
    advapi32::SHIMS,
    dsound::SHIMS,
    ddraw::SHIMS,
    dinput::SHIMS,
    winmm::SHIMS,
];

/// The binding for a dispatch slot, matched on DLL and function name.
///
/// `tables` is the target's resolution order: the generic tables first, the
/// port's own after them, and **later wins**, so a port overrides one function
/// by registering it again instead of forking a table.
pub fn binding(tables: &'static [&'static [Binding]], slot: &Slot) -> Option<&'static Binding> {
    tables
        .iter()
        .rev()
        .flat_map(|table| table.iter())
        .find(|binding| {
            binding.name == slot.name && binding.dll.eq_ignore_ascii_case(&slot.dll)
        })
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
