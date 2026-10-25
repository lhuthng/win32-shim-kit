//! The browser boundary: a flat C ABI over one [`Session`], for any PE32 game.
//!
//! This is what makes the kit produce something a page can run: `cdylib` builds
//! `host_wasm.wasm`, a page instantiates it with an empty import object, and the
//! exports below are the whole contract. Nothing here names a game.
//!
//! # No generated table
//!
//! A port used to generate `[Slot; N]` per executable and compile it in. Here the
//! table comes from the image itself: [`wh_load`] parses the import directory,
//! sorts its entries by IAT address, and matches every entry against the shim
//! tables by `DLL!Function`. Call-site arity is not part of that table (`None`),
//! which is fine because stdcall restores the stack from the saved `esp` and each
//! shim declares its own signature; what a generated table adds is a
//! cross-check, not correctness.
//!
//! # Pointers
//!
//! Every pointer is an offset into the module's linear memory. The page writes
//! the executable into [`wh_alloc`]'s buffer and then calls [`wh_load`]; it does
//! the same for a file (name followed by its bytes, in one allocation).
//!
//! Outcome codes are [`Outcome::code`]: 0 budget reached, 1 halted, 2 host call,
//! 3 fault, and -1 when there is no session. A fault or an unanswered import is
//! returned as text through [`wh_message_ptr`], never as a trap.

use std::cell::{Cell, RefCell};

use host::dispatch::{Slot, Table};
use host::pe;
use host::session::{Outcome, Session};
use host::target::{self, Target};

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// The bytes [`wh_load`] was given, kept so [`wh_reset`] can map them again.
    static ORIGINAL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    /// The target built from the loaded image: its name, screen and table.
    static TARGET: Cell<Option<&'static Target>> = const { Cell::new(None) };
    /// What the guest should call itself. Set before loading; `program.exe` if not.
    static EXE_NAME: RefCell<String> = const { RefCell::new(String::new()) };
    /// The screen to seed before the guest asks for its own display mode.
    static SCREEN: Cell<(u16, u16, u8)> = const { Cell::new((640, 480, 8)) };
    static MESSAGE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static LAST_SLOT: Cell<i32> = const { Cell::new(-1) };
    static NOTICES_SHOWN: Cell<u32> = const { Cell::new(0) };
}

fn set_message(text: &str) {
    MESSAGE.with(|message| {
        let mut message = message.borrow_mut();
        message.clear();
        message.extend_from_slice(text.as_bytes());
    });
}

/// Copies a string into the module's static lifetime.
///
/// A target lives as long as the loaded session does, and the engine asks for
/// `&'static` facts. One leak per load, never per reset, is the price of that
/// signature without threading a lifetime through the whole engine.
fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

/// The import surface of the image: one slot per IAT entry, in IAT order.
fn table_for(image: &pe::Image) -> Table {
    let mut imports = image.imports.clone();
    imports.sort_by_key(|import| import.iat_va);
    let slots: Vec<Slot> = imports
        .iter()
        .map(|import| Slot {
            dll: leak(&import.dll),
            name: leak(&import.name),
            iat_va: import.iat_va,
            // The host does not read the call sites; every shim carries its own
            // signature and stdcall returns from the saved esp.
            args: None,
            sites: 0,
            agreement: 0,
            ambiguous: 0,
        })
        .collect();
    Table {
        slots: Box::leak(slots.into_boxed_slice()),
    }
}

fn build_target(image: &pe::Image) -> &'static Target {
    let name = EXE_NAME.with(|name| {
        let name = name.borrow();
        if name.is_empty() {
            "program.exe".to_string()
        } else {
            name.clone()
        }
    });
    let screen = SCREEN.with(Cell::get);
    Box::leak(Box::new(Target {
        name: "wasm",
        exe_name: leak(&name),
        screen,
        table: table_for(image),
        shims: host::shim::TABLES,
        provided_files: &[],
    }))
}

fn target() -> &'static Target {
    TARGET.with(|target| target.get()).unwrap_or(&target::DEFAULT)
}

/// Names the executable about to be loaded, so `GetModuleFileNameA` answers with
/// the build actually booted. Call before [`wh_load`]; 0, or -1 when empty.
#[no_mangle]
pub extern "C" fn wh_set_exe_name(ptr: *const u8, len: u32) -> i32 {
    if ptr.is_null() || len == 0 {
        set_message("no executable name was supplied");
        return -1;
    }
    // Safety: the page passes a pointer from `wh_alloc` and the length it wrote.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) };
    let Ok(name) = std::str::from_utf8(bytes) else {
        set_message("the executable name is not UTF-8");
        return -1;
    };
    EXE_NAME.with(|slot| *slot.borrow_mut() = name.to_owned());
    0
}

/// Seeds the screen before the guest creates its own: width, height, bits.
/// Call before [`wh_load`]; 0, or -1 for a size the host cannot take.
#[no_mangle]
pub extern "C" fn wh_set_screen(width: u32, height: u32, depth: u32) -> i32 {
    if width == 0 || width > u16::MAX as u32 || height == 0 || height > u16::MAX as u32 {
        set_message("the screen size is out of range");
        return -1;
    }
    if depth != 8 && depth != 16 && depth != 24 && depth != 32 {
        set_message("the screen depth must be 8, 16, 24 or 32");
        return -1;
    }
    SCREEN.with(|screen| screen.set((width as u16, height as u16, depth as u8)));
    0
}

/// A buffer the page fills with whatever a `wh_*` call needs to see.
///
/// There is one buffer, reused and grown as required, so everything a single
/// call needs must go into one allocation: a second [`wh_alloc`] returns the
/// same memory.
#[no_mangle]
pub extern "C" fn wh_alloc(len: u32) -> *mut u8 {
    SCRATCH.with(|scratch| {
        let mut scratch = scratch.borrow_mut();
        if scratch.len() < len as usize {
            scratch.resize(len as usize, 0);
        }
        scratch.as_mut_ptr()
    })
}

/// Parses and maps `len` bytes at `ptr`, building its import table from the
/// image. Returns 0, or -1 with a message.
#[no_mangle]
pub extern "C" fn wh_load(ptr: *const u8, len: u32) -> i32 {
    if ptr.is_null() || len == 0 {
        set_message("no executable bytes were supplied");
        return -1;
    }
    // Safety: the page passes a pointer it received from `wh_alloc` and a length
    // that fits that buffer.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len as usize) }.to_vec();
    let image = match pe::Image::parse(bytes.clone()) {
        Ok(image) => image,
        Err(error) => {
            set_message(&error);
            return -1;
        }
    };
    let target = build_target(&image);
    TARGET.with(|slot| slot.set(Some(target)));
    match Session::load_for(bytes.clone(), target) {
        Ok(session) => {
            ORIGINAL.with(|original| *original.borrow_mut() = bytes);
            SESSION.with(|cell| *cell.borrow_mut() = Some(session));
            LAST_SLOT.with(|slot| slot.set(-1));
            NOTICES_SHOWN.with(|shown| shown.set(0));
            set_message("");
            0
        }
        Err(error) => {
            set_message(&error);
            -1
        }
    }
}

/// Remaps the loaded executable with the table it already built: registers reset,
/// files dropped (the page provides them again).
#[no_mangle]
pub extern "C" fn wh_reset() -> i32 {
    let original = ORIGINAL.with(|original| original.borrow().clone());
    if original.is_empty() {
        set_message("no executable loaded");
        return -1;
    }
    match Session::load_for(original, target()) {
        Ok(session) => {
            SESSION.with(|cell| *cell.borrow_mut() = Some(session));
            LAST_SLOT.with(|slot| slot.set(-1));
            NOTICES_SHOWN.with(|shown| shown.set(0));
            set_message("");
            0
        }
        Err(error) => {
            set_message(&error);
            -1
        }
    }
}

/// Runs up to `budget` instructions and reports why it stopped.
#[no_mangle]
pub extern "C" fn wh_run(budget: u32) -> i32 {
    let outcome = SESSION.with(|cell| cell.borrow_mut().as_mut().map(|session| session.run(budget)));
    let Some(outcome) = outcome else {
        set_message("no executable loaded");
        return -1;
    };

    match &outcome {
        Outcome::HostCall(call) => {
            LAST_SLOT.with(|slot| slot.set(call.slot as i32));
            set_message(&outcome.describe(target().table));
        }
        Outcome::Fault(_) => {
            LAST_SLOT.with(|slot| slot.set(-1));
            set_message(&outcome.describe(target().table));
        }
        Outcome::Halted => {
            LAST_SLOT.with(|slot| slot.set(-1));
            let exit = SESSION.with(|cell| cell.borrow().as_ref().and_then(Session::exit_code));
            set_message(&match exit {
                Some(code) => format!("halted after ExitProcess({code})"),
                None => "halted".to_string(),
            });
        }
        Outcome::Steps => {}
    }
    outcome.code()
}

/// Hands the guest a file before it runs. The name and its bytes must be one
/// allocation: `wh_alloc(name_len + data_len)`.
#[no_mangle]
pub extern "C" fn wh_provide_file(
    name: *const u8,
    name_len: u32,
    data: *const u8,
    data_len: u32,
) -> i32 {
    if name.is_null() || name_len == 0 {
        set_message("a file needs a name");
        return -1;
    }
    if data.is_null() && data_len != 0 {
        set_message("the file contents are missing");
        return -1;
    }
    // Safety: both pointers are inside the module's memory, written by the page.
    let name = unsafe { std::slice::from_raw_parts(name, name_len as usize) };
    let path = String::from_utf8_lossy(name).into_owned();
    let bytes = if data_len == 0 {
        Vec::new()
    } else {
        unsafe { std::slice::from_raw_parts(data, data_len as usize) }.to_vec()
    };
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.host.files.provide(&path, bytes);
            0
        }
        None => {
            set_message("no executable loaded");
            -1
        }
    })
}

/// Stands in for a file the guest only measures: it gets a length and no bytes.
///
/// This is how a disc marker or a launcher is answered without inventing a
/// device: the guest stats the file, sees the size it expects, and never reads
/// it. `name` is a normal `wh_alloc` buffer.
#[no_mangle]
pub extern "C" fn wh_provide_sized(name: *const u8, name_len: u32, length: u32) -> i32 {
    if name.is_null() || name_len == 0 {
        set_message("a marker needs a name");
        return -1;
    }
    // Safety: the page passes a pointer from `wh_alloc` and its length.
    let name = unsafe { std::slice::from_raw_parts(name, name_len as usize) };
    let path = String::from_utf8_lossy(name).into_owned();
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.host.files.provide_sized(&path, length);
            0
        }
        None => {
            set_message("no executable loaded");
            -1
        }
    })
}

/// The guest's clock in milliseconds, pushed before each chunk. Without it the
/// host falls back to instruction counts, and the guest's timers follow how fast
/// the interpreter happens to run instead of real time.
#[no_mangle]
pub extern "C" fn wh_set_time(milliseconds: f64) -> i32 {
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.set_time_ms(milliseconds.max(0.0) as u64);
            0
        }
        None => -1,
    })
}

#[no_mangle]
pub extern "C" fn wh_entry() -> u32 {
    SESSION.with(|cell| cell.borrow().as_ref().map_or(0, Session::entry))
}

#[no_mangle]
pub extern "C" fn wh_pc() -> u32 {
    SESSION.with(|cell| cell.borrow().as_ref().map_or(0, Session::pc))
}

#[no_mangle]
pub extern "C" fn wh_instructions() -> f64 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0.0, |session| session.instructions() as f64)
    })
}

/// The slot of the last unanswered import, or -1.
#[no_mangle]
pub extern "C" fn wh_slot() -> i32 {
    LAST_SLOT.with(Cell::get)
}

/// How many imports the loaded image has, which is also the length of its table.
#[no_mangle]
pub extern "C" fn wh_import_count() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.target.table.len() as u32)
    })
}

#[no_mangle]
pub extern "C" fn wh_message_ptr() -> *const u8 {
    MESSAGE.with(|message| message.borrow().as_ptr())
}

#[no_mangle]
pub extern "C" fn wh_message_len() -> u32 {
    MESSAGE.with(|message| message.borrow().len() as u32)
}

/// Writes the next notice the page has not printed into the message buffer:
/// 1 when one was written, 0 when there are none left, -1 with no session.
#[no_mangle]
pub extern "C" fn wh_notice_next() -> i32 {
    SESSION.with(|cell| {
        let session = cell.borrow();
        let Some(session) = session.as_ref() else {
            return -1;
        };
        let index = NOTICES_SHOWN.with(Cell::get) as usize;
        let Some(text) = session.host.notices.get(index) else {
            return 0;
        };
        NOTICES_SHOWN.with(|shown| shown.set(index as u32 + 1));
        set_message(text);
        1
    })
}

/// Offset of the primary surface's pixels in linear memory, or 0 before the
/// guest has a display. The page reads width by height indexed bytes from
/// `wh_memory_ptr() + wh_framebuffer_offset()` and maps them through the palette.
#[no_mangle]
pub extern "C" fn wh_framebuffer_offset() -> u32 {
    SESSION.with(|cell| {
        cell.borrow().as_ref().map_or(0, |session| {
            session
                .host
                .ddraw
                .framebuffer()
                .map_or(0, |(buffer, _, _)| buffer.wrapping_sub(pe::IMAGE_BASE))
        })
    })
}

#[no_mangle]
pub extern "C" fn wh_framebuffer_width() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.ddraw.width())
    })
}

#[no_mangle]
pub extern "C" fn wh_framebuffer_height() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.ddraw.height())
    })
}

#[no_mangle]
pub extern "C" fn wh_framebuffer_pitch() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.ddraw.framebuffer_pitch())
    })
}

/// Frames the guest has presented, so the page redraws only on change.
#[no_mangle]
pub extern "C" fn wh_flips() -> f64 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0.0, |session| session.host.ddraw.flips() as f64)
    })
}

/// Palette rewrites, which happen without a flip during a fade.
#[no_mangle]
pub extern "C" fn wh_palette_version() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.ddraw.palette_revision())
    })
}

/// Palette entry `index` as `0x00RRGGBB`, or 0 out of range.
#[no_mangle]
pub extern "C" fn wh_palette(index: u32) -> u32 {
    if index > 255 {
        return 0;
    }
    SESSION.with(|cell| {
        cell.borrow().as_ref().map_or(0, |session| {
            let [red, green, blue] = session.host.ddraw.palette()[index as usize];
            (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue)
        })
    })
}

/// The queued audio events, `wh_audio_event_count` records of eleven
/// little-endian dwords: kind, voice, channels, rate, frames, volume, pan, flags,
/// bytes, frequency, bits. Kinds are the engine's `EVENT_*` constants.
#[no_mangle]
pub extern "C" fn wh_audio_events_ptr() -> *const u8 {
    SESSION.with(|cell| {
        cell.borrow().as_ref().map_or(std::ptr::null(), |session| {
            session.host.audio.events().as_ptr().cast::<u8>()
        })
    })
}

#[no_mangle]
pub extern "C" fn wh_audio_event_count() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.audio.events().len() as u32)
    })
}

/// Frees the events and any released voice's PCM. The page copies what it needs
/// first.
#[no_mangle]
pub extern "C" fn wh_audio_events_clear() {
    SESSION.with(|cell| {
        if let Some(session) = cell.borrow_mut().as_mut() {
            session.host.audio.clear_events();
        }
    });
}

#[no_mangle]
pub extern "C" fn wh_audio_pcm_ptr(voice: u32) -> *const u8 {
    SESSION.with(|cell| {
        cell.borrow().as_ref().map_or(std::ptr::null(), |session| {
            session.host.audio.pcm(voice).as_ptr()
        })
    })
}

#[no_mangle]
pub extern "C" fn wh_audio_pcm_len(voice: u32) -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.audio.pcm(voice).len() as u32)
    })
}

#[no_mangle]
pub extern "C" fn wh_audio_plays() -> f64 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0.0, |session| session.host.audio.plays() as f64)
    })
}

// ---- input ---------------------------------------------------------------

#[no_mangle]
pub extern "C" fn wh_key(dik: u32, down: u32) -> i32 {
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.host.key(dik as u8, down != 0);
            0
        }
        None => -1,
    })
}

#[no_mangle]
pub extern "C" fn wh_pointer(x: i32, y: i32) -> i32 {
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.host.pointer(x, y);
            0
        }
        None => -1,
    })
}

#[no_mangle]
pub extern "C" fn wh_pointer_move(dx: i32, dy: i32) -> i32 {
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.host.pointer_move(dx, dy);
            0
        }
        None => -1,
    })
}

#[no_mangle]
pub extern "C" fn wh_pointer_button(button: u32, down: u32) -> i32 {
    SESSION.with(|cell| match cell.borrow_mut().as_mut() {
        Some(session) => {
            session.host.pointer_button(button as u8, down != 0);
            0
        }
        None => -1,
    })
}

// ---- memory, for the canvas and the diagnostics panel ---------------------

#[no_mangle]
pub extern "C" fn wh_memory_ptr() -> *const u8 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(std::ptr::null(), |session| session.memory.as_slice().as_ptr())
    })
}

#[no_mangle]
pub extern "C" fn wh_memory_len() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.memory.as_slice().len() as u32)
    })
}

#[no_mangle]
pub extern "C" fn wh_heap_used() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.heap_used())
    })
}

#[no_mangle]
pub extern "C" fn wh_heap_free() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.heap_free())
    })
}

/// The pixels `Flip` presents onto, or 0. A page reads its frame here; it can
/// differ from the surface the guest is currently drawing into.
#[no_mangle]
pub extern "C" fn wh_primary_buffer() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|session| session.host.ddraw.primary_buffer())
            .unwrap_or(0)
    })
}

/// The pixels the guest is drawing into before a `Flip`, or 0.
#[no_mangle]
pub extern "C" fn wh_back_buffer() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .and_then(|session| session.host.ddraw.back_buffer())
            .unwrap_or(0)
    })
}

/// How often the guest locked the surface whose buffer is `buffer`, and the
/// geometry that goes with it. Used by the checks to tell "the guest drew into a
/// buffer nobody reads" from "the guest never drew".
#[no_mangle]
pub extern "C" fn wh_surface_locks(buffer: u32) -> u32 {
    SESSION.with(|cell| {
        let session = cell.borrow();
        let Some(session) = session.as_ref() else {
            return 0;
        };
        session
            .host
            .ddraw
            .surface_buffers()
            .into_iter()
            .find(|(candidate, ..)| *candidate == buffer)
            .map(|(.., locks)| locks)
            .unwrap_or(0)
    })
}

#[no_mangle]
pub extern "C" fn wh_surface_count() -> u32 {
    SESSION.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or(0, |session| session.host.surface_count())
    })
}
