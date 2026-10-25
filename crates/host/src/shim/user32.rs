//! The `USER32.dll` subset the boot needs: one window class, one window, and
//! the queries that describe them.
//!
//! WinMain registers a class, creates the window the game draws into, shows it,
//! and from then on only asks the window about its rectangle and its extra
//! bytes. That is all this module implements. There is no window manager here:
//! the browser page *is* the window, the guest never sees a border or a title
//! bar, and the client rectangle `CreateWindowExA` was given is the whole
//! window. `AdjustWindowRectEx` therefore leaves its rectangle untouched, which
//! is exactly what a borderless window does.
//!
//! # What is deliberately absent
//!
//! # The message pump
//!
//! The frame loop is a `PeekMessageA` loop: ask for a message, and draw a frame
//! when there is none. With no key or pointer event the queue is empty, so the
//! guest draws every iteration -- the same behaviour the title screen had before
//! input existed. With events, `PeekMessageA` returns them and `DispatchMessageA`
//! calls the window procedure the class registered, through the session's guest
//! call mechanism ([`crate::shim::GuestCall`]).
//!
//! `GetMessageA` cannot block: the page cannot interrupt a call, so a real block
//! would freeze the tab. Instead of the `WM_QUIT` this host used to answer, it
//! now hands back `WM_NULL` when the queue is empty -- a message every window
//! ignores, which keeps the guest alive long enough for the next event instead of
//! ending the process.
//!
//! The dialog calls (`DialogBoxParamA`, `EndDialog`, `SendDlgItemMessageA`,
//! `GetDlgItemTextA`, `SetDlgItemTextA`) still have no shim: they need a drawn
//! dialog, which is the M3 text work. `DrawTextA` waits for the same fonts. The
//! session stops and names whichever comes first, which is the diagnostic this
//! file grows from.
//!
//! `MessageBoxA` is the one call that changes the guest's flow rather than
//! answering a query: it records the caption and text as a host notice and
//! returns `IDOK`, so a boot that reports a problem says so in the log instead
//! of hanging on a dialog the page cannot draw yet.

use std::collections::HashMap;

use crate::shim::{Action, Binding, Call, GuestCall, Host};
use crate::input;
use crate::Result;

use Action::Return;

const USER32: &str = "USER32.dll";

/// Handles this module hands out. They are small, aligned, and disjoint from
/// every other handle the host makes, so a log line reads at a glance.
const DESKTOP_WINDOW: u32 = 0x0001_0000;
const FIRST_WINDOW: u32 = 0x0001_0100;
const FIRST_DC: u32 = 0x0001_0200;
const ARROW_CURSOR: u32 = 0x0001_0300;
const APPLICATION_ICON: u32 = 0x0001_0400;

/// `IDC_ARROW` and `IDI_APPLICATION`: the first predefined cursor and icon.
const FIRST_CURSOR_ID: u32 = 32512;
const FIRST_ICON_ID: u32 = 32512;

/// Offsets inside a 32-bit `WNDCLASSA`, which is 40 bytes and packs on 4.
const WNDCLASS_PROC: u32 = 4;
const WNDCLASS_NAME: u32 = 36;

/// `SPI_GETWORKAREA`: the area the game centres itself against.
const SPI_GETWORKAREA: u32 = 0x0030;

/// `GWL_STYLE` and `GWL_EXSTYLE`, which are negative selectors, not indices.
const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;

/// `IDOK`, the only message-box result the guest can get back for now.
const IDOK: u32 = 1;

/// `PeekMessageA`'s remove flag; zero is `PM_NOREMOVE`.
const PM_REMOVE: u32 = 1;

/// `WM_NULL`: the message `GetMessageA` answers with when nothing is queued.
const WM_NULL: u32 = 0x0000;

/// `WM_ACTIVATE` with `WA_ACTIVE`, `WM_SETFOCUS` and `WM_ACTIVATEAPP`: what a
/// window receives when it becomes the one the user is looking at.
const WM_SETFOCUS: u32 = 0x0007;
const WM_ACTIVATE: u32 = 0x0006;
const WM_ACTIVATEAPP: u32 = 0x001c;
const WA_ACTIVE: u32 = 1;

/// `CW_USEDEFAULT`, which the game passes for the position *and* the size of its
/// window: it lets the OS pick, then sets the display mode itself.
const CW_USEDEFAULT: i32 = i32::MIN;

/// The surface the page presents, which is what "the default window" means here.
const SURFACE_WIDTH: i32 = 640;
const SURFACE_HEIGHT: i32 = 480;

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "AdjustWindowRectEx", arg_bytes: 16, shim: adjust_window_rect },
    Binding { dll: USER32, name: "CreateWindowExA", arg_bytes: 48, shim: create_window_ex },
    Binding { dll: USER32, name: "DefWindowProcA", arg_bytes: 16, shim: def_window_proc },
    Binding { dll: USER32, name: "DispatchMessageA", arg_bytes: 4, shim: dispatch_message },
    Binding { dll: USER32, name: "FindWindowA", arg_bytes: 8, shim: find_window },
    Binding { dll: USER32, name: "GetAsyncKeyState", arg_bytes: 4, shim: get_async_key_state },
    Binding { dll: USER32, name: "GetCursorPos", arg_bytes: 4, shim: get_cursor_pos },
    Binding { dll: USER32, name: "GetDC", arg_bytes: 4, shim: get_dc },
    Binding { dll: USER32, name: "GetDesktopWindow", arg_bytes: 0, shim: get_desktop_window },
    Binding { dll: USER32, name: "GetMenu", arg_bytes: 4, shim: get_menu },
    Binding { dll: USER32, name: "GetMessageA", arg_bytes: 16, shim: get_message },
    Binding { dll: USER32, name: "GetWindowDC", arg_bytes: 4, shim: get_dc },
    Binding { dll: USER32, name: "GetWindowLongA", arg_bytes: 8, shim: get_window_long },
    Binding { dll: USER32, name: "GetWindowRect", arg_bytes: 8, shim: get_window_rect },
    Binding { dll: USER32, name: "LoadCursorA", arg_bytes: 8, shim: load_cursor },
    Binding { dll: USER32, name: "LoadIconA", arg_bytes: 8, shim: load_icon },
    Binding { dll: USER32, name: "MessageBoxA", arg_bytes: 16, shim: message_box },
    Binding { dll: USER32, name: "PeekMessageA", arg_bytes: 20, shim: peek_message },
    Binding { dll: USER32, name: "PostQuitMessage", arg_bytes: 4, shim: post_quit_message },
    Binding { dll: USER32, name: "RegisterClassA", arg_bytes: 4, shim: register_class },
    Binding { dll: USER32, name: "ReleaseDC", arg_bytes: 8, shim: release_dc },
    Binding { dll: USER32, name: "SetCursorPos", arg_bytes: 8, shim: set_cursor_pos },
    Binding { dll: USER32, name: "SetRect", arg_bytes: 20, shim: set_rect },
    Binding { dll: USER32, name: "SetWindowLongA", arg_bytes: 12, shim: set_window_long },
    Binding { dll: USER32, name: "SetWindowPos", arg_bytes: 28, shim: set_window_pos },
    Binding { dll: USER32, name: "ShowCursor", arg_bytes: 4, shim: show_cursor },
    Binding { dll: USER32, name: "ShowWindow", arg_bytes: 8, shim: show_window },
    Binding { dll: USER32, name: "SystemParametersInfoA", arg_bytes: 16, shim: system_parameters_info },
    Binding { dll: USER32, name: "TranslateMessage", arg_bytes: 4, shim: translate_message },
    Binding { dll: USER32, name: "UpdateWindow", arg_bytes: 4, shim: update_window },
];

/// A window the guest created.
struct Window {
    class: String,
    /// The window procedure the class was registered with. Kept because
    /// `DispatchMessageA` will call it once the pump exists.
    proc_address: u32,
    style: u32,
    /// `left, top, right, bottom`, in the guest's coordinate space.
    rect: [i32; 4],
}

/// Everything `user32` remembers between calls: the registered classes, the
/// windows they created, and the pointer state.
#[derive(Default)]
pub struct Windows {
    classes: Vec<(String, u32)>,
    windows: HashMap<u32, Window>,
    /// The guest's window: the first one it created, which is where input goes.
    main: u32,
    /// Whether the activation messages have been queued, so they are delivered
    /// once rather than on every `ShowWindow`.
    activated: bool,
    /// Extra bytes the guest stashed with `SetWindowLongA`, keyed by window and
    /// selector. `GWL_STYLE` is answered from the window itself.
    extra: HashMap<(u32, i32), u32>,
    next_window: u32,
    next_dc: u32,
    cursor: (i32, i32),
    cursor_visible: i32,
    /// The screen the port presents, when it named one. `None` means the
    /// conventional VGA mode this emulation assumes.
    screen: Option<(i32, i32)>,
}

impl Windows {
    pub fn with_screen(width: i32, height: i32) -> Self {
        Self {
            screen: Some((width, height)),
            ..Self::default()
        }
    }

    /// The surface the page presents: the target's, or the assumed mode.
    pub fn screen(&self) -> (i32, i32) {
        self.screen.unwrap_or((SURFACE_WIDTH, SURFACE_HEIGHT))
    }
}

impl Windows {
    fn register(&mut self, class: String, proc_address: u32) -> u32 {
        self.classes.push((class, proc_address));
        self.classes.len() as u32
    }

    fn create(&mut self, class: String, proc_address: u32, style: u32, rect: [i32; 4]) -> u32 {
        let handle = FIRST_WINDOW + self.next_window * 4;
        self.next_window += 1;
        if self.main == 0 {
            self.main = handle;
        }
        self.windows.insert(
            handle,
            Window {
                class,
                proc_address,
                style,
                rect,
            },
        );
        handle
    }

    /// The window the page's input belongs to.
    pub fn main_window(&self) -> u32 {
        self.main
    }

    /// Moves the pointer, which `GetCursorPos` reports.
    pub fn set_cursor(&mut self, x: i32, y: i32) {
        self.cursor = (x, y);
    }

    /// Where the pointer is, which is what `GetCursorPos` answers.
    pub fn cursor(&self) -> (i32, i32) {
        self.cursor
    }

    /// The window procedure the guest registered for a window, if it has one.
    pub fn proc_address(&self, handle: u32) -> Option<u32> {
        self.windows
            .get(&handle)
            .map(|window| window.proc_address)
            .filter(|address| *address != 0)
    }

    fn rect(&self, handle: u32) -> Option<[i32; 4]> {
        if handle == DESKTOP_WINDOW {
            // The desktop is the whole surface the page presents.
            let (width, height) = self.screen();
            return Some([0, 0, width, height]);
        }
        self.windows.get(&handle).map(|window| window.rect)
    }

    fn set_rect(&mut self, handle: u32, rect: [i32; 4]) {
        if let Some(window) = self.windows.get_mut(&handle) {
            window.rect = rect;
        }
    }

    fn style(&self, handle: u32) -> Option<u32> {
        self.windows.get(&handle).map(|window| window.style)
    }

    fn next_dc(&mut self) -> u32 {
        let handle = FIRST_DC + self.next_dc * 4;
        self.next_dc += 1;
        handle
    }

    /// One line about a window, for the diagnostics log. It reads every field a
    /// window records, so a window the guest created is fully visible in a trace.
    pub fn describe(&self, handle: u32) -> Option<String> {
        let window = self.windows.get(&handle)?;
        Some(format!(
            "window {handle:#010x} \"{}\" style {:#010x} proc {:#010x} rect {}x{} at {},{}",
            window.class,
            window.style,
            window.proc_address,
            window.rect[2] - window.rect[0],
            window.rect[3] - window.rect[1],
            window.rect[0],
            window.rect[1]
        ))
    }
}

// ---- window classes and windows ------------------------------------------

fn register_class(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let class = call.arg(0)?;
    if class == 0 {
        return Err("RegisterClassA without a WNDCLASSA".to_string());
    }
    let proc_address = call.memory.read_u32(class.wrapping_add(WNDCLASS_PROC))?;
    let name = call
        .memory
        .read_cstr(call.memory.read_u32(class.wrapping_add(WNDCLASS_NAME))?)?;
    // The atom is not a real atom; it only has to be non-zero and stable.
    Ok(Return(host.windows.register(name, proc_address)))
}

fn create_window_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let class = optional_cstr(call, 1)?;
    let title = optional_cstr(call, 2)?;
    let style = call.arg(3)?;
    // The game asks for `CW_USEDEFAULT` in every coordinate, so the window the
    // host reports is the surface itself; a zero size would be a window nobody
    // could see, and Windows does not create one either.
    let x = defaulted(call.arg(4)? as i32, 0);
    let y = defaulted(call.arg(5)? as i32, 0);
    let width = defaulted(call.arg(6)? as i32, SURFACE_WIDTH);
    let height = defaulted(call.arg(7)? as i32, SURFACE_HEIGHT);
    let proc_address = host
        .windows
        .classes
        .iter()
        .find(|(registered, _)| registered == &class)
        .map(|(_, proc_address)| *proc_address)
        .unwrap_or(0);
    let handle = host.windows.create(
        class,
        proc_address,
        style,
        [
            x,
            y,
            x.saturating_add(width),
            y.saturating_add(height),
        ],
    );
    if let Some(line) = host.windows.describe(handle) {
        host.notice(line);
    }
    if !title.is_empty() {
        host.notice(format!("window title \"{title}\""));
    }
    Ok(Return(handle))
}

/// There is one guest in this host, so the answer is always "no other copy".
/// A second instance would need a second session, not a window search.
fn find_window(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn show_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The page is visible when it runs the guest, so the window is on screen.
    // Windows follows that with the activation messages, and the guest's window
    // procedure reads them: it is how a game learns it has the user's attention.
    let handle = call.arg(0)?;
    if !host.windows.activated && handle != 0 {
        host.windows.activated = true;
        host.input.post(handle, WM_ACTIVATE, WA_ACTIVE, 0);
        host.input.post(handle, WM_SETFOCUS, 0, 0);
        host.input.post(handle, WM_ACTIVATEAPP, 1, 0);
    }
    Ok(Return(1))
}

fn update_window(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// Takes the first queued message matching the filter, or reports that there is
/// none so the guest draws another frame.
///
/// `PeekMessageA(lpMsg, hwnd, filterMin, filterMax, remove)` -- the message
/// structure comes *first*, before the window, which is the order Win32 declares
/// and the order the call sites push.
fn peek_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let window = call.arg(1)?;
    let min = call.arg(2)?;
    let max = call.arg(3)?;
    let remove = call.arg(4)? & PM_REMOVE != 0;

    let Some(index) = host.input.find(window, min, max) else {
        return Ok(Return(0));
    };
    let message = match if remove {
        host.input.remove(index)
    } else {
        host.input.get(index)
    } {
        Some(message) => message,
        None => return Ok(Return(0)),
    };
    if destination != 0 {
        call.write(destination, &message.to_bytes())?;
    }
    Ok(Return(1))
}

/// `GetMessageA` cannot wait, so an empty queue answers with `WM_NULL`: the
/// guest keeps running and the next event finds it in its loop, instead of the
/// process ending on a message that never came.
fn get_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // `GetMessageA(lpMsg, hwnd, filterMin, filterMax)`, same order as the peek.
    let destination = call.arg(0)?;
    let window = call.arg(1)?;
    let min = call.arg(2)?;
    let max = call.arg(3)?;

    let message = host.input.take(window, min, max).unwrap_or(input::Message {
        hwnd: window,
        message: WM_NULL,
        wparam: 0,
        lparam: 0,
        time: 0,
        x: 0,
        y: 0,
    });
    if destination != 0 {
        call.write(destination, &message.to_bytes())?;
    }
    // Always 1: a queued message was returned, or the placeholder that keeps the
    // guest's own loop alive.
    Ok(Return(1))
}

/// Generates the `WM_CHAR` a key-down message implies, which is what a dialog's
/// edit control reads instead of the virtual key.
fn translate_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    if address == 0 {
        return Ok(Return(0));
    }
    let window = call.memory.read_u32(address)?;
    let message = call.memory.read_u32(address + 4)?;
    let wparam = call.memory.read_u32(address + 8)?;
    Ok(Return(u32::from(host.input.translate(window, message, wparam))))
}

/// Runs the window procedure the class registered, through the session's guest
/// call mechanism; the import finishes with the procedure's own return value.
fn dispatch_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    if address == 0 {
        return Ok(Return(0));
    }
    let window = call.memory.read_u32(address)?;
    let message = call.memory.read_u32(address + 4)?;
    let wparam = call.memory.read_u32(address + 8)?;
    let lparam = call.memory.read_u32(address + 12)?;
    if window == 0 {
        // `WM_QUIT` from a thread queue: there is no window and no procedure.
        return Ok(Return(0));
    }
    match host.windows.proc_address(window) {
        Some(entry) => {
            host.guest_call = Some(GuestCall {
                entry,
                args: [window, message, wparam, lparam],
            });
            Ok(Return(0))
        }
        None => Ok(Return(0)),
    }
}

fn post_quit_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let code = call.arg(0)?;
    host.notice(format!("PostQuitMessage({code}): the guest asked to quit"));
    Ok(Action::Exit(code))
}

fn get_desktop_window(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(DESKTOP_WINDOW))
}

fn get_window_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let destination = call.arg(1)?;
    let Some(rect) = host.windows.rect(handle) else {
        // An unknown window is a guest error, not a shape this host invents a
        // rectangle for.
        return Ok(Return(0));
    };
    if destination != 0 {
        write_rect_at(call, destination, rect)?;
    }
    Ok(Return(1))
}

fn set_window_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let x = call.arg(1)? as i32;
    let y = call.arg(2)? as i32;
    let width = call.arg(3)? as i32;
    let height = call.arg(4)? as i32;
    host.windows.set_rect(
        handle,
        [
            x,
            y,
            x.saturating_add(width),
            y.saturating_add(height),
        ],
    );
    Ok(Return(1))
}

/// Substitutes the host's choice where the guest asked for `CW_USEDEFAULT`.
fn defaulted(value: i32, fallback: i32) -> i32 {
    if value == CW_USEDEFAULT {
        fallback
    } else {
        value
    }
}

/// A string argument, where null means "absent" rather than "read address 0".
fn optional_cstr(call: &Call<'_>, index: u32) -> Result<String> {
    if call.arg(index)? == 0 {
        return Ok(String::new());
    }
    call.cstr(index)
}

fn adjust_window_rect(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The rehost has no border and no title bar, so the client rectangle the
    // guest asked for is already the window rectangle.
    Ok(Return(1))
}

fn set_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    if destination == 0 {
        return Ok(Return(0));
    }
    let left = call.arg(1)? as i32;
    let top = call.arg(2)? as i32;
    let right = call.arg(3)? as i32;
    let bottom = call.arg(4)? as i32;
    write_rect_at(call, destination, [left, top, right, bottom])?;
    Ok(Return(1))
}

fn set_window_long(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let selector = call.arg(1)? as i32;
    let value = call.arg(2)?;
    let previous = window_long(host, handle, selector);
    match selector {
        GWL_STYLE => {
            // `SetWindowLongA(hwnd, GWL_STYLE, style)` is how a Win32 game
            // switches between windowed and borderless: the guest stores the
            // value and asks for it back, so the store has to be real.
            host.windows
                .windows
                .entry(handle)
                .and_modify(|window| window.style = value);
        }
        GWL_EXSTYLE => {}
        _ => {
            host.windows
                .extra
                .insert((handle, selector), value);
        }
    }
    Ok(Return(previous))
}

fn get_window_long(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let selector = call.arg(1)? as i32;
    Ok(Return(window_long(host, handle, selector)))
}

fn window_long(host: &Host, handle: u32, selector: i32) -> u32 {
    match selector {
        GWL_STYLE => host.windows.style(handle).unwrap_or(0),
        // There are no extended styles: no always-on-top, no layered window.
        GWL_EXSTYLE => 0,
        _ => host
            .windows
            .extra
            .get(&(handle, selector))
            .copied()
            .unwrap_or(0),
    }
}

fn get_menu(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The image carries no menu resource; the game draws its own.
    Ok(Return(0))
}

fn def_window_proc(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

// ---- device contexts, cursor and input -----------------------------------

fn get_dc(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.windows.next_dc()))
}

fn release_dc(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn show_cursor(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Windows keeps a display counter that hides the pointer at zero; the same
    // arithmetic here means a `ShowCursor(FALSE)` from the game hides the page's
    // own pointer too once input is wired to it.
    let show = call.arg(0)?;
    host.windows.cursor_visible += if show != 0 { 1 } else { -1 };
    Ok(Return(host.windows.cursor_visible as u32))
}

fn get_cursor_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let point = call.arg(0)?;
    if point != 0 {
        let (x, y) = host.windows.cursor;
        call.write_u32(point, x as u32)?;
        call.write_u32(point.wrapping_add(4), y as u32)?;
    }
    Ok(Return(1))
}

fn set_cursor_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (x, y) = (call.arg(0)? as i32, call.arg(1)? as i32);
    host.windows.cursor = (x, y);
    // There is one cursor on Windows: where `SetCursorPos` put it is where
    // `GetCursorPos` reports it and where the mouse messages carry it. Keeping a
    // second position for the pointer let a warping guest and the page drift
    // apart, so a click landed somewhere other than the cursor it drew.
    host.input.mouse = (x, y);
    Ok(Return(1))
}

fn get_async_key_state(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Bit 15 is the key's state now, bit 0 whether it was pressed since the last
    // call. The second bit is consumed by the read, as on Windows.
    let key = call.arg(0)?;
    Ok(Return(u32::from(host.input.async_state(key as u8))))
}

fn load_cursor(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(predefined(call.arg(1)?, FIRST_CURSOR_ID, ARROW_CURSOR)))
}

fn load_icon(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(predefined(call.arg(1)?, FIRST_ICON_ID, APPLICATION_ICON)))
}

/// A predefined resource id resolves to a handle; a named resource does not,
/// because the image has no resource section to load it from.
fn predefined(id: u32, first_id: u32, base: u32) -> u32 {
    if id >= first_id && id < first_id + 64 {
        base + (id - first_id) * 4
    } else {
        0
    }
}

fn system_parameters_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let action = call.arg(0)?;
    let target = call.arg(2)?;
    match action {
        SPI_GETWORKAREA => {
            // The work area is the surface: no taskbar, no other windows.
            if target != 0 {
                let (width, height) = host.windows.screen();
                write_rect_at(call, target, [0, 0, width, height])?;
            }
            Ok(Return(1))
        }
        _ => {
            // Anything else is a query this host cannot answer; saying so is
            // better than writing zeroes the game would act on.
            host.last_error = 87; // ERROR_INVALID_PARAMETER
            Ok(Return(0))
        }
    }
}

fn message_box(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = call.cstr(1)?;
    let caption = call.cstr(2)?;
    host.notice(format!("MessageBoxA \"{caption}\": {text}"));
    Ok(Return(IDOK))
}

fn write_rect_at(call: &mut Call<'_>, destination: u32, rect: [i32; 4]) -> Result<()> {
    for (index, value) in rect.iter().enumerate() {
        call.write_u32(destination.wrapping_add(index as u32 * 4), *value as u32)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{self, LEFT};
    use crate::memory::{GuestMemory, STACK_BASE};
    use crate::shim::GuestCall;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    fn call_shim(
        host: &mut Host,
        memory: &mut GuestMemory,
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("{name} is implemented"));
        let args = STACK_BASE + 0x2000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call)
    }

    #[test]
    fn moving_the_cursor_moves_the_one_cursor_every_view_reads() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The game parks the cursor in the middle of the screen and reads it back
        // with `GetCursorPos`; there is one cursor, so the pointer the page drives
        // and the messages carry have to follow the same point.
        assert_eq!(
            call_shim(&mut host, &mut memory, "SetCursorPos", &[320, 240]).unwrap(),
            Return(1)
        );
        assert_eq!(host.windows.cursor(), (320, 240));
        assert_eq!(host.input.mouse, (320, 240));

        let point = STACK_BASE + 0x2400;
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetCursorPos", &[point]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(point).unwrap(), 320);
        assert_eq!(memory.read_u32(point + 4).unwrap(), 240);

        // A locked pointer's movement is added to that parked point, so it can
        // keep going left instead of walking into the window edge and stopping.
        host.pointer_move(-40, 0);
        assert_eq!(host.windows.cursor(), (280, 240));
        assert_eq!(host.input.mouse, (280, 240));
    }

    #[test]
    fn a_registered_class_remembers_its_window_procedure() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A WNDCLASSA with a name pointer at +36 and a wndproc at +4.
        let class = STACK_BASE + 0x3000;
        let name = STACK_BASE + 0x3100;
        memory.write_bytes(name, b"DoramonClass\0").unwrap();
        memory.write_u32(class + WNDCLASS_PROC, 0x0040_1234).unwrap();
        memory.write_u32(class + WNDCLASS_NAME, name).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "RegisterClassA", &[class]).unwrap(),
            Return(1)
        );

        // Creating a window with that class name picks up the procedure.
        let class_name = STACK_BASE + 0x3200;
        let title = STACK_BASE + 0x3300;
        memory.write_bytes(class_name, b"DoramonClass\0").unwrap();
        memory.write_bytes(title, b"Doramon\0").unwrap();
        let created = call_shim(
            &mut host,
            &mut memory,
            "CreateWindowExA",
            &[0, class_name, title, 0x10cf_0000, 0, 0, 640, 480, 0, 0, 0, 0],
        )
        .unwrap();
        let Return(handle) = created else {
            panic!("CreateWindowExA returns a handle");
        };
        assert!(handle > 0);
        let description = host.windows.describe(handle).expect("the window exists");
        assert!(description.contains("DoramonClass"), "{description}");
        assert!(description.contains("0x00401234"), "{description}");
        assert!(description.contains("640x480"), "{description}");
    }

    #[test]
    fn a_window_reports_its_client_rectangle() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let name = STACK_BASE + 0x3400;
        memory.write_bytes(name, b"Board\0").unwrap();
        let Return(handle) = call_shim(
            &mut host,
            &mut memory,
            "CreateWindowExA",
            &[0, name, 0, 0, 100, 50, 640, 480, 0, 0, 0, 0],
        )
        .unwrap() else {
            panic!("a handle");
        };

        let rect = STACK_BASE + 0x3500;
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetWindowRect", &[handle, rect]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(rect).unwrap(), 100);
        assert_eq!(memory.read_u32(rect + 4).unwrap(), 50);
        assert_eq!(memory.read_u32(rect + 8).unwrap(), 740);
        assert_eq!(memory.read_u32(rect + 12).unwrap(), 530);

        // The desktop window is the whole surface.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GetWindowRect",
                &[DESKTOP_WINDOW, rect]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(rect + 8).unwrap(), 640);
        assert_eq!(memory.read_u32(rect + 12).unwrap(), 480);
    }

    #[test]
    fn a_defaulted_window_is_the_surface() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The game passes CW_USEDEFAULT for all four coordinates, which is what
        // the diagnostic log showed: a window the OS sizes.
        let used = CW_USEDEFAULT as u32;
        let name = STACK_BASE + 0x3550;
        memory.write_bytes(name, b"Sample\0").unwrap();
        let Return(handle) = call_shim(
            &mut host,
            &mut memory,
            "CreateWindowExA",
            &[0, name, name, 0x8008_0000, used, used, used, used, 0, 0, 0, 0],
        )
        .unwrap() else {
            panic!("a handle");
        };
        let rect = STACK_BASE + 0x3560;
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetWindowRect", &[handle, rect]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(rect).unwrap(), 0);
        assert_eq!(memory.read_u32(rect + 4).unwrap(), 0);
        assert_eq!(memory.read_u32(rect + 8).unwrap(), 640);
        assert_eq!(memory.read_u32(rect + 12).unwrap(), 480);
    }

    #[test]
    fn no_other_instance_is_running() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let class = STACK_BASE + 0x3600;
        memory.write_bytes(class, b"DoramonClass\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "FindWindowA", &[class, 0]).unwrap(),
            Return(0)
        );
    }

    #[test]
    fn the_style_survives_a_set_and_get() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let name = STACK_BASE + 0x3700;
        memory.write_bytes(name, b"Board\0").unwrap();
        let Return(handle) = call_shim(
            &mut host,
            &mut memory,
            "CreateWindowExA",
            &[0, name, 0, 0x00cf_0000, 0, 0, 640, 480, 0, 0, 0, 0],
        )
        .unwrap() else {
            panic!("a handle");
        };

        // Setting GWL_STYLE returns the previous value and stores the new one.
        let style = GWL_STYLE as u32;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetWindowLongA",
                &[handle, style, 0x8000_0000]
            )
            .unwrap(),
            Return(0x00cf_0000)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetWindowLongA", &[handle, style]).unwrap(),
            Return(0x8000_0000)
        );

        // Any other selector is a plain per-window dword.
        let object = (-21_i32) as u32; // GWL_USERDATA
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetWindowLongA",
                &[handle, object, 0x1234_5678]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetWindowLongA", &[handle, object]).unwrap(),
            Return(0x1234_5678)
        );
    }

    #[test]
    fn set_rect_writes_four_signed_dwords() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let rect = STACK_BASE + 0x3800;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetRect",
                &[rect, (-4_i32) as u32, 5, 100, 200]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(rect).unwrap(), (-4_i32) as u32);
        assert_eq!(memory.read_u32(rect + 4).unwrap(), 5);
        assert_eq!(memory.read_u32(rect + 8).unwrap(), 100);
        assert_eq!(memory.read_u32(rect + 12).unwrap(), 200);
    }

    #[test]
    fn the_work_area_is_the_surface() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let rect = STACK_BASE + 0x3900;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SystemParametersInfoA",
                &[SPI_GETWORKAREA, 0, rect, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(rect + 8).unwrap(), 640);
        assert_eq!(memory.read_u32(rect + 12).unwrap(), 480);

        // An action this host cannot answer reports an error instead of
        // writing zeroes the guest would act on.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SystemParametersInfoA",
                &[0x0001_0001, 0, rect, 0]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, 87);
    }

    #[test]
    fn a_message_box_is_recorded_not_swallowed() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let text = STACK_BASE + 0x3a00;
        let caption = STACK_BASE + 0x3b00;
        memory.write_bytes(text, b"DirectDraw failed\0").unwrap();
        memory.write_bytes(caption, b"Doramon\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "MessageBoxA", &[0, text, caption, 0]).unwrap(),
            Return(IDOK)
        );
        assert_eq!(host.notices.len(), 1);
        assert!(host.notices[0].contains("DirectDraw failed"), "{:?}", host.notices);
    }

    #[test]
    fn predefined_resources_are_handles_and_named_ones_are_not() {
        assert_eq!(predefined(32512, FIRST_CURSOR_ID, ARROW_CURSOR), ARROW_CURSOR);
        assert_eq!(
            predefined(32513, FIRST_CURSOR_ID, ARROW_CURSOR),
            ARROW_CURSOR + 4
        );
        // A name pointer is not a resource id; the image has no resources.
        assert_eq!(predefined(0x0040_1234, FIRST_ICON_ID, APPLICATION_ICON), 0);
    }

    /// Creates a window whose class has `proc`, and returns its handle.
    fn window_with_proc(
        host: &mut Host,
        memory: &mut GuestMemory,
        proc_address: u32,
    ) -> u32 {
        let class = STACK_BASE + 0x3c00;
        let name = STACK_BASE + 0x3c80;
        memory.write_bytes(name, b"GameClass\0").unwrap();
        memory.write_u32(class + WNDCLASS_PROC, proc_address).unwrap();
        memory.write_u32(class + WNDCLASS_NAME, name).unwrap();
        call_shim(host, memory, "RegisterClassA", &[class]).unwrap();
        let created = call_shim(
            host,
            memory,
            "CreateWindowExA",
            &[0, name, 0, 0, 0, 0, 640, 480, 0, 0, 0, 0],
        )
        .unwrap();
        match created {
            Return(handle) => handle,
            other => panic!("CreateWindowExA returned {other:?}"),
        }
    }

    #[test]
    fn a_queued_message_is_peeked_and_only_removed_when_asked() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        host.key(0x1e, true);

        let message = STACK_BASE + 0x3d00;
        // PM_NOREMOVE: the message is copied but stays queued.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "PeekMessageA",
                &[message, window, 0, 0, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(message).unwrap(), window);
        assert_eq!(memory.read_u32(message + 4).unwrap(), input::WM_KEYDOWN);
        assert_eq!(memory.read_u32(message + 8).unwrap(), u32::from(b'A'));
        assert_eq!(host.input.len(), 1);

        // PM_REMOVE: it is taken.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "PeekMessageA",
                &[message, window, 0, 0, PM_REMOVE]
            )
            .unwrap(),
            Return(1)
        );
        assert!(host.input.is_empty());

        // An empty queue is the answer that lets the guest draw a frame.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "PeekMessageA",
                &[message, window, 0, 0, PM_REMOVE]
            )
            .unwrap(),
            Return(0)
        );
    }

    #[test]
    fn the_message_structure_is_msgs_layout() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        host.pointer(100, 50);
        host.pointer_button(LEFT, true);
        let message = STACK_BASE + 0x3e00;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "PeekMessageA",
                &[message, window, 0, 0, PM_REMOVE]
            )
            .unwrap(),
            Return(1)
        );
        // The pointer message comes first: it happened before the press.
        assert_eq!(memory.read_u32(message + 4).unwrap(), input::WM_MOUSEMOVE);
        assert_eq!(i32::from_le_bytes(memory.read_bytes(message + 20, 4).unwrap().try_into().unwrap()), 100);
        assert_eq!(i32::from_le_bytes(memory.read_bytes(message + 24, 4).unwrap().try_into().unwrap()), 50);

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "PeekMessageA",
                &[message, window, 0, 0, PM_REMOVE]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(message + 4).unwrap(), input::WM_LBUTTONDOWN);
        assert_eq!(memory.read_u32(message + 8).unwrap(), 1, "MK_LBUTTON");
    }

    #[test]
    fn get_message_keeps_the_guest_alive_when_nothing_is_queued() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        let message = STACK_BASE + 0x3f00;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GetMessageA",
                &[message, window, 0, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(message + 4).unwrap(), WM_NULL);

        // A real message is returned and removed.
        host.key(0x1e, true);
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GetMessageA",
                &[message, window, 0, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(message + 4).unwrap(), input::WM_KEYDOWN);
        assert!(host.input.is_empty());
    }

    #[test]
    fn translate_makes_a_character_from_a_key_down() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        let message = STACK_BASE + 0x4000;
        memory.write_u32(message, window).unwrap();
        memory.write_u32(message + 4, input::WM_KEYDOWN).unwrap();
        memory.write_u32(message + 8, u32::from(b'H')).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "TranslateMessage", &[message]).unwrap(),
            Return(1)
        );
        let character = host.input.next().expect("a character message");
        assert_eq!(character.message, input::WM_CHAR);
        assert_eq!(character.wparam, u32::from(b'h'));
    }

    #[test]
    fn dispatch_asks_the_session_to_run_the_window_procedure() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        let message = STACK_BASE + 0x4100;
        memory.write_u32(message, window).unwrap();
        memory.write_u32(message + 4, input::WM_KEYDOWN).unwrap();
        memory.write_u32(message + 8, u32::from(b'A')).unwrap();
        memory.write_u32(message + 12, 0x001e_0001).unwrap();

        assert_eq!(
            call_shim(&mut host, &mut memory, "DispatchMessageA", &[message]).unwrap(),
            Return(0)
        );
        assert_eq!(
            host.guest_call,
            Some(GuestCall {
                entry: 0x0045_7da3,
                args: [window, input::WM_KEYDOWN, u32::from(b'A'), 0x001e_0001],
            })
        );
    }

    #[test]
    fn async_key_state_answers_by_virtual_key() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        host.key(0x1e, true);
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetAsyncKeyState", &[u32::from(b'A')]).unwrap(),
            Return(0x8001)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetAsyncKeyState", &[u32::from(b'B')]).unwrap(),
            Return(0)
        );
    }

    #[test]
    fn the_cursor_counter_adds_up() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowCursor", &[0]).unwrap(),
            Return((-1_i32) as u32)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowCursor", &[1]).unwrap(),
            Return(0)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowCursor", &[1]).unwrap(),
            Return(1)
        );
    }
}
