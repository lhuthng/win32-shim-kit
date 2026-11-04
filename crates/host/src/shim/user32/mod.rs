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
//! `GetMessageA` blocks the way Windows does: an empty queue parks the thread on
//! the message queue until a producer queues a message, with a short deadline so
//! a pump that drives its frame from this loop still wakes on time alone. The
//! page cannot interrupt a parked call, but its frame loop re-enters the session
//! every frame and its input exports wake the queue, which is the same shape as
//! a blocked pump the OS wakes with events.
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

/// The rest of the module is split by subject.
///
/// A game from this era imports around a hundred and sixty entry points from
/// `USER32.dll`, and a delta that size is unreadable in one file. Each submodule
/// files its own [`Binding`] table under the same `USER32.dll`, so a function
/// lives next to the code that answers it and the dispatch table is still one
/// DLL's. What they share -- the windows themselves, menus, scroll state, timers
/// -- is [`Windows`], defined below.
pub mod dialog;
pub mod menu;
pub mod misc;
pub mod paint;
pub mod rect;
pub mod state;
pub mod window;

const USER32: &str = "USER32.dll";

/// Handles this module hands out. They are small, aligned, and disjoint from
/// every other handle the host makes, so a log line reads at a glance.
const DESKTOP_WINDOW: u32 = 0x0001_0000;
const FIRST_WINDOW: u32 = 0x0001_0100;
const ARROW_CURSOR: u32 = 0x0001_0300;
const APPLICATION_ICON: u32 = 0x0001_0400;

/// Menu handles, above the window handles.
const FIRST_MENU: u32 = 0x0001_0800;

/// Hook handles, above the menus: a `SetWindowsHookExA` handle is an index into
/// the installed list, so a log line can say which hook a guest unhooks.
const FIRST_HOOK: u32 = 0x0001_0900;

/// The selector a window's menu bar is filed under in the extra-bytes table.
///
/// `GWL_*` selectors are the negative numbers Windows documents, so this one is
/// outside them on purpose: it only has to be a value no guest asks for, and a
/// guest that did would be storing a number under it anyway.
const GWL_MENU: i32 = -1000;

/// `WM_TIMER`: what a due `SetTimer` timer posts.
const WM_TIMER: u32 = 0x0113;

/// Handles for bitmaps loaded out of the image's resource tree.
const FIRST_BITMAP: u32 = 0x0001_0500;

/// `LoadImageA`'s types, and the resource type bitmap resources are filed under.
const IMAGE_BITMAP: u32 = 0;
const IMAGE_ICON: u32 = 1;
const IMAGE_CURSOR: u32 = 2;
const RT_BITMAP: u32 = 2;
/// `LR_LOADFROMFILE`: the name is a path in the file table, not a resource.
const LR_LOADFROMFILE: u32 = 0x0010;

/// `IDC_ARROW` and `IDI_APPLICATION`: the first predefined cursor and icon.
const FIRST_CURSOR_ID: u32 = 32512;
const FIRST_ICON_ID: u32 = 32512;

/// Offsets inside a 32-bit `WNDCLASSA`, which is 40 bytes and packs on 4.
const WNDCLASS_PROC: u32 = 4;
const WNDCLASS_NAME: u32 = 36;

/// `SPI_GETWORKAREA`: the area the game centres itself against.
const SPI_GETWORKAREA: u32 = 0x0030;

/// `DEVMODEA`'s display fields at their documented offsets: the structure is
/// 156 bytes and every member after `dmFields` lands where the `union` in the
/// middle puts it, so the offsets are named rather than derived.
const DEVMODE_FIELDS: u32 = 40;
const DEVMODE_BITS_PER_PEL: u32 = 104;
const DEVMODE_PELS_WIDTH: u32 = 108;
const DEVMODE_PELS_HEIGHT: u32 = 112;
const DEVMODE_DISPLAY_FREQUENCY: u32 = 120;

/// The `dmFields` bits for the four values above.
const DM_BITSPERPEL: u32 = 0x0004_0000;
const DM_PELSWIDTH: u32 = 0x0008_0000;
const DM_PELSHEIGHT: u32 = 0x0010_0000;
const DM_DISPLAYFREQUENCY: u32 = 0x0040_0000;

/// `ENUM_CURRENT_SETTINGS`: ask for the mode in use rather than the nth one.
const ENUM_CURRENT_SETTINGS: u32 = 0xffff_ffff;
/// The rate a browser canvas is presented at, which is the one mode's.
const DISPLAY_FREQUENCY: u32 = 60;
/// `DISP_CHANGE_SUCCESSFUL`.
const DISP_CHANGE_SUCCESSFUL: u32 = 0;

/// `GWL_STYLE` and `GWL_EXSTYLE`, which are negative selectors, not indices.
const GWL_STYLE: i32 = -16;
const GWL_EXSTYLE: i32 = -20;

/// `GWL_WNDPROC`: the selector a window procedure is stored under. A window's
/// own procedure, once something subclasses it, is what every message goes to.
const GWL_WNDPROC: i32 = -4;

/// `WH_CBT` and `HCBT_CREATEWND`: the hook kind Windows calls before a window is
/// created, and the code it is called with. MFC installs one for exactly one
/// reason -- the class it registers carries `DefWindowProcA`, and the hook is
/// what puts `AfxWndProc` in its place -- so without it a frame's `CreateEx`
/// fails and the application quits before it has a window.
const WH_CBT: i32 = 5;
const HCBT_CREATEWND: u32 = 3;

/// `WM_NCCREATE` and `WM_CREATE`: delivered, in that order, before
/// `CreateWindowExA` returns, to the window's own procedure.
const WM_NCCREATE: u32 = 0x0081;
const WM_CREATE: u32 = 0x0001;

/// A 32-bit `CREATESTRUCT`: twelve members, every one a `DWORD`, so 48 bytes.
const CREATESTRUCT_BYTES: u32 = 48;

/// `IDOK`, the only message-box result the guest can get back for now.
const IDOK: u32 = 1;

/// `PeekMessageA`'s remove flag; zero is `PM_NOREMOVE`.
const PM_REMOVE: u32 = 1;

/// `WM_NULL`: the message `GetMessageA` answers with when nothing is queued.
const WM_NULL: u32 = 0x0000;

/// `WS_VISIBLE`: a window created with it is on screen from the moment it
/// exists, so creating it activates it without a separate `ShowWindow`.
const WS_VISIBLE: u32 = 0x1000_0000;

/// `WM_ACTIVATE` with `WA_ACTIVE`, `WM_SETFOCUS` and `WM_ACTIVATEAPP`: what a
/// window receives when it becomes the one the user is looking at.
const WM_SETFOCUS: u32 = 0x0007;
const WM_PAINT: u32 = 0x000f;
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
    Binding { dll: USER32, name: "AdjustWindowRect", arg_bytes: 12, shim: adjust_window_rect },
    Binding { dll: USER32, name: "AdjustWindowRectEx", arg_bytes: 16, shim: adjust_window_rect },
    Binding { dll: USER32, name: "ChangeDisplaySettingsA", arg_bytes: 8, shim: change_display_settings },
    Binding { dll: USER32, name: "EnumDisplaySettingsA", arg_bytes: 12, shim: enum_display_settings },
    Binding { dll: USER32, name: "CreateWindowExA", arg_bytes: 48, shim: create_window_ex },
    Binding { dll: USER32, name: "DefWindowProcA", arg_bytes: 16, shim: def_window_proc },
    Binding { dll: USER32, name: "DispatchMessageA", arg_bytes: 4, shim: dispatch_message },
    Binding { dll: USER32, name: "FindWindowA", arg_bytes: 8, shim: find_window },
    Binding { dll: USER32, name: "FindWindowW", arg_bytes: 8, shim: find_window },
    Binding { dll: USER32, name: "MessageBoxW", arg_bytes: 16, shim: message_box_w },
    Binding { dll: USER32, name: "GetAsyncKeyState", arg_bytes: 4, shim: get_async_key_state },
    Binding { dll: USER32, name: "GetCursorPos", arg_bytes: 4, shim: get_cursor_pos },
    Binding { dll: USER32, name: "GetDC", arg_bytes: 4, shim: get_dc },
    Binding { dll: USER32, name: "GetDesktopWindow", arg_bytes: 0, shim: get_desktop_window },
    Binding { dll: USER32, name: "GetForegroundWindow", arg_bytes: 0, shim: get_foreground_window },
    Binding { dll: USER32, name: "GetMenu", arg_bytes: 4, shim: get_menu },
    Binding { dll: USER32, name: "GetMessageA", arg_bytes: 16, shim: get_message },
    Binding { dll: USER32, name: "GetWindowDC", arg_bytes: 4, shim: get_dc },
    Binding { dll: USER32, name: "GetWindowLongA", arg_bytes: 8, shim: get_window_long },
    Binding { dll: USER32, name: "GetWindowRect", arg_bytes: 8, shim: get_window_rect },
    Binding { dll: USER32, name: "LoadImageA", arg_bytes: 24, shim: load_image },
    Binding { dll: USER32, name: "LoadStringA", arg_bytes: 16, shim: load_string },
    Binding { dll: USER32, name: "SetCapture", arg_bytes: 4, shim: set_capture },
    Binding { dll: USER32, name: "ReleaseCapture", arg_bytes: 0, shim: release_capture },
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
    Binding { dll: USER32, name: "CloseClipboard", arg_bytes: 0, shim: close_clipboard },
    Binding { dll: USER32, name: "CreateWindowExW", arg_bytes: 48, shim: create_window_ex_w },
    Binding { dll: USER32, name: "DefWindowProcW", arg_bytes: 16, shim: def_window_proc },
    Binding { dll: USER32, name: "RegisterClassW", arg_bytes: 4, shim: register_class_w },
    Binding { dll: USER32, name: "GetClientRect", arg_bytes: 8, shim: get_client_rect },
    Binding { dll: USER32, name: "ClientToScreen", arg_bytes: 8, shim: client_to_screen },
    Binding { dll: USER32, name: "InvalidateRect", arg_bytes: 12, shim: invalidate_rect },
    Binding { dll: USER32, name: "IsIconic", arg_bytes: 4, shim: is_iconic },
    Binding { dll: USER32, name: "GetKeyState", arg_bytes: 4, shim: get_key_state },
    Binding { dll: USER32, name: "MapVirtualKeyA", arg_bytes: 8, shim: map_virtual_key },
    Binding { dll: USER32, name: "PtInRect", arg_bytes: 12, shim: pt_in_rect },
    Binding { dll: USER32, name: "SendMessageA", arg_bytes: 16, shim: send_message },
    Binding { dll: USER32, name: "SetCursor", arg_bytes: 4, shim: set_cursor },
    Binding { dll: USER32, name: "DestroyWindow", arg_bytes: 4, shim: destroy_window },
    Binding { dll: USER32, name: "GetClipboardData", arg_bytes: 4, shim: get_clipboard_data },
    Binding { dll: USER32, name: "CharNextA", arg_bytes: 4, shim: char_next },
    Binding { dll: USER32, name: "CharUpperA", arg_bytes: 4, shim: char_upper },
    Binding { dll: USER32, name: "GetKeyboardType", arg_bytes: 4, shim: get_keyboard_type },
    Binding { dll: USER32, name: "GetSystemMetrics", arg_bytes: 4, shim: get_system_metrics },
    Binding { dll: USER32, name: "OpenClipboard", arg_bytes: 4, shim: open_clipboard },
    Binding { dll: USER32, name: "PostMessageA", arg_bytes: 16, shim: post_message },
    Binding { dll: USER32, name: "RegisterClassExA", arg_bytes: 4, shim: register_class_ex },
    Binding { dll: USER32, name: "SetClipboardData", arg_bytes: 8, shim: set_clipboard_data },
    Binding { dll: USER32, name: "SetWindowTextA", arg_bytes: 8, shim: set_window_text },
    Binding { dll: USER32, name: "ValidateRect", arg_bytes: 8, shim: validate_rect },
    // `wsprintfA` is variadic and declared `WINAPIV`, so it is cdecl: the shim's
    // declared bytes are how far its arguments may extend, and the caller pops.
    Binding { dll: USER32, name: "wsprintfA", arg_bytes: 52, shim: wsprintf_a },
    Binding { dll: USER32, name: "wsprintfW", arg_bytes: 52, shim: wsprintf_w },
];

/// `WNDCLASSEXA`: the class name and procedure, four bytes further along than in
/// the ANSI structure because of the leading `cbSize`.
const WNDCLASSEX_PROC: u32 = 8;
const WNDCLASSEX_NAME: u32 = 40;

fn register_class_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let class = call.arg(0)?;
    if class == 0 {
        return Err("RegisterClassExA without a WNDCLASSEXA".to_string());
    }
    let proc_address = call.memory.read_u32(class.wrapping_add(WNDCLASSEX_PROC))?;
    let name = call
        .memory
        .read_cstr(call.memory.read_u32(class.wrapping_add(WNDCLASSEX_NAME))?)?;
    Ok(Return(host.windows.register(name, proc_address)))
}

fn destroy_window(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The page owns the one window the guest sees; destroying it does not make
    // the machine invisible, and the guest uses the answer only to check success.
    Ok(Return(1))
}

/// `IsIconic(hwnd)`: the canvas is never minimised, whatever the tab is doing.
fn is_iconic(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `GetSystemMetrics(index)`: the screen the target presents for the two sizes,
/// and a zero for everything the browser does not model (caption height, border
/// width, and the rest) -- the same zeroes a bare display reports.
fn get_system_metrics(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (width, height) = host.windows.screen();
    // The metrics a windowed guest measures itself with. A zero is never a real
    // answer to any of them: a program that asks how tall a caption is and is
    // told zero lays its client area out as if the window had no frame.
    Ok(Return(match call.arg(0)? {
        0 => width as u32,                                        // SM_CXSCREEN
        1 => height as u32,                                       // SM_CYSCREEN
        2 => 17,                                                  // SM_CXVSCROLL
        3 => 17,                                                  // SM_CYHSCROLL
        4 => 19,                                                  // SM_CYCAPTION
        5 | 6 => 1,                                               // SM_CXBORDER, SM_CYBORDER
        7 | 8 => 4,                                               // SM_CXDLGFRAME, SM_CYDLGFRAME
        11 | 12 => 32,                                            // SM_CXICON, SM_CYICON
        13 | 14 => 32,                                            // SM_CXCURSOR, SM_CYCURSOR
        15 => 1,                                                  // SM_CYMENU
        16 => (width - 8).max(0) as u32,                          // SM_CXFULLSCREEN
        17 => (height - 27).max(0) as u32,                        // SM_CYFULLSCREEN
        32 | 33 => 4,                                             // SM_CXFRAME, SM_CYFRAME
        36 | 37 => 4,                                             // SM_CXDOUBLECLK, SM_CYDOUBLECLK
        43 => 3,                                                  // SM_CMOUSEBUTTONS
        68 | 69 => 4,                                             // SM_CXDRAG, SM_CYDRAG
        80 => 1,                                                  // SM_CMONITORS
        _ => 0,
    }))
}

/// `EnumDisplaySettingsA(device, mode, devmode)`: the one mode this machine has.
///
/// The page presents a fixed surface, so the list holds exactly the session's
/// screen at 32 bits: mode 0 and `ENUM_CURRENT_SETTINGS` fill it in, and every
/// later index answers FALSE -- which is how a guest learns the list ended rather
/// than that the call failed.
fn enum_display_settings(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let mode = call.arg(1)?;
    let devmode = call.arg(2)?;
    if devmode == 0 || !(mode == 0 || mode == ENUM_CURRENT_SETTINGS) {
        return Ok(Return(0));
    }
    let (width, height) = host.windows.screen();
    let fields = DM_BITSPERPEL | DM_PELSWIDTH | DM_PELSHEIGHT | DM_DISPLAYFREQUENCY;
    call.write_u32(devmode.wrapping_add(DEVMODE_FIELDS), fields)?;
    call.write_u32(devmode.wrapping_add(DEVMODE_BITS_PER_PEL), 32)?;
    call.write_u32(devmode.wrapping_add(DEVMODE_PELS_WIDTH), width as u32)?;
    call.write_u32(devmode.wrapping_add(DEVMODE_PELS_HEIGHT), height as u32)?;
    call.write_u32(
        devmode.wrapping_add(DEVMODE_DISPLAY_FREQUENCY),
        DISPLAY_FREQUENCY,
    )?;
    Ok(Return(1))
}

/// `ChangeDisplaySettingsA(mode, flags)`: success without changing anything.
///
/// A page cannot take the visitor's desktop, and the surface the game draws into
/// is already the size it asked for, so the mode change is a no-op that reports
/// the success a guest needs to carry on rather than fall back to windowed mode.
fn change_display_settings(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if std::env::var("SHIMKIT_LOG_USER").is_ok() && host.notices.len() < 600 {
        host.notice(
            "ChangeDisplaySettingsA: the canvas is already the requested mode".to_string(),
        );
    }
    let _ = call.arg(0)?;
    Ok(Return(DISP_CHANGE_SUCCESSFUL))
}

/// `CharNextA(text)`: the next character, which in the single-byte codepage this
/// host reports is one byte on. A lead byte would be two, and nothing here
/// claims a DBCS codepage.
fn char_next(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = call.arg(0)?;
    if text == 0 || call.memory.read_u8(text)? == 0 {
        return Ok(Return(text));
    }
    Ok(Return(text + 1))
}

/// `CharUpperA(text)`: ASCII uppercase, in place.
///
/// Win32 overloads the one entry point: a value below `0x10000` is a character
/// to uppercase rather than a pointer to a string, and the answer is that
/// character either way.
fn char_upper(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = call.arg(0)?;
    if text < 0x0001_0000 {
        let character = (text & 0xff) as u8;
        return Ok(Return(u32::from(character.to_ascii_uppercase())));
    }
    let mut at = text;
    loop {
        let character = call.memory.read_u8(at)?;
        if character == 0 {
            break;
        }
        let upper = character.to_ascii_uppercase();
        if upper != character {
            call.write(at, &[upper])?;
        }
        at = at.wrapping_add(1);
    }
    Ok(Return(text))
}

/// `GetKeyboardType(flag)`: an enhanced 101-key PC, which is the keyboard every
/// browser reports. The three flags ask for the type, the subtype and the number
/// of function keys, and those are the three answers.
fn get_keyboard_type(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(match call.arg(0)? {
        0 => 1,
        1 => 4,
        _ => 12,
    }))
}

fn post_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.input.post(call.arg(0)?, call.arg(1)?, call.arg(2)?, call.arg(3)?);
    host.wake_message_waiters();
    Ok(Return(1))
}

/// `SendMessageA(hwnd, message, wparam, lparam)`: the window procedure's own
/// answer, because the procedure runs before this import returns.
///
/// The routine is entered through the session's guest-call mechanism, and the
/// shim is resumed when it returns so the answer is the import's answer. A
/// window with no guest procedure -- one whose class still holds the default
/// procedure -- answers zero, which is what `DefWindowProcA` says for the
/// messages a guest sends to a window it did not subclass.
fn send_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if host.resumed {
        return Ok(Return(host.guest_result));
    }
    let window = call.arg(0)?;
    let message = call.arg(1)?;
    let wparam = call.arg(2)?;
    let lparam = call.arg(3)?;
    let entry = host.windows.proc_address(window).unwrap_or(0);
    if !is_guest_code(host, entry) {
        return Ok(Return(0));
    }
    host.guest_call = Some(GuestCall {
        entry,
        args: [window, message, wparam, lparam],
        answer: None,
        resume: true,
    });
    Ok(Return(0))
}

fn set_window_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let text = call.cstr(1)?;
    host.windows.set_text(window, text);
    Ok(Return(1))
}

fn validate_rect(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // Nothing paints the guest's window through GDI, so there is no invalid
    // region to clear; the guest's own drawing is the display.
    Ok(Return(1))
}

fn open_clipboard(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn close_clipboard(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn set_clipboard_data(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The data handle is handed straight back, which is what a successful
    // `SetClipboardData` returns. The browser's clipboard could hold it, but a
    // game copying a score is not the user's clipboard to overwrite.
    Ok(Return(call.arg(1)?))
}

fn get_clipboard_data(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `wsprintfA(buffer, format, ...)`: the CRT's small formatter, used for window
/// titles and scores. `wsprintfW` is the same formatter writing UTF-16.
///
/// The arguments are read straight from the guest stack, which is why this shim
/// declares a window rather than a signature: how many there are is written in
/// the format itself. Widths and the integer/signed/string conversions are
/// supported; floating point is not, and a `%f` leaves the value unread rather
/// than guessing at a guest's stack layout.
fn wsprintf_a(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    format_into(host, call, false)
}

fn wsprintf_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    format_into(host, call, true)
}

/// The body both `wsprintfA` and `wsprintfW` run: the same format language and
/// the same argument window, with only the width of a character changing -- and
/// with it the width of the `%s` and `%c` it formats.
fn format_into(_: &mut Host, call: &mut Call<'_>, wide: bool) -> Result<Action> {
    let destination = call.arg(0)?;
    let format = if wide {
        call.wstr(call.arg(1)?)?
    } else {
        call.cstr(1)?
    };
    let mut output = String::new();
    let mut argument = 2_u32;
    let mut characters = format.chars().peekable();

    while let Some(character) = characters.next() {
        if character != '%' {
            output.push(character);
            continue;
        }
        if characters.peek() == Some(&'%') {
            characters.next();
            output.push('%');
            continue;
        }
        let mut left = false;
        let mut zero = false;
        let mut width = 0_usize;
        loop {
            match characters.peek().copied() {
                Some('-') => {
                    left = true;
                    characters.next();
                }
                Some('0') => {
                    zero = true;
                    characters.next();
                }
                Some('#') | Some('+') | Some(' ') => {
                    characters.next();
                }
                Some(digit) if digit.is_ascii_digit() => {
                    width = width * 10 + digit.to_digit(10).unwrap_or(0) as usize;
                    characters.next();
                }
                _ => break,
            }
        }
        if characters.peek() == Some(&'.') {
            characters.next();
            while characters.peek().is_some_and(|digit| digit.is_ascii_digit()) {
                characters.next();
            }
        }
        // The modifier decides how wide a `%s` or `%c` is, not the format: in a
        // wide format `%hs` names an ANSI string and `%s` a wide one, and in an
        // ANSI format it is the other way round. Ignoring it reads a path as
        // UTF-16 and formats it as mojibake.
        let mut hinted = None;
        while let Some(&modifier) = characters.peek() {
            match modifier {
                'l' | 'w' => hinted = Some(true),
                'h' => hinted = Some(false),
                'L' | 'I' | '6' | '4' => {}
                _ => break,
            }
            characters.next();
        }
        let Some(specifier) = characters.next() else {
            break;
        };
        let value = call.arg(argument)?;
        argument += 1;
        let text = match specifier {
            'd' | 'i' => (value as i32).to_string(),
            'u' => value.to_string(),
            'x' => format!("{value:x}"),
            'X' => format!("{value:X}"),
            'c' => {
                let unit = hinted.unwrap_or(wide);
                char::from_u32(value & if unit { 0xffff } else { 0xff })
                    .unwrap_or('?')
                    .to_string()
            }
            // A capital `%S` is the opposite width to the format's own.
            's' | 'S' => {
                // A null string formats as the empty string, as the CRT does.
                if value == 0 {
                    String::new()
                } else {
                    let string_is_wide = if specifier == 'S' {
                        !wide
                    } else {
                        hinted.unwrap_or(wide)
                    };
                    if string_is_wide {
                        call.wstr(value)?
                    } else {
                        call.memory.read_cstr(value)?
                    }
                }
            }
            _ => continue,
        };
        // A field's width counts characters, which differs from the byte count
        // for anything outside ASCII.
        let length = text.chars().count();
        if length >= width {
            output.push_str(&text);
        } else if left {
            output.push_str(&text);
            output.extend(std::iter::repeat_n(' ', width - length));
        } else {
            output.extend(std::iter::repeat_n(if zero { '0' } else { ' ' }, width - length));
            output.push_str(&text);
        }
    }

    if destination != 0 {
        if wide {
            let mut at = destination;
            for unit in output.encode_utf16() {
                call.write_u16(at, unit)?;
                at = at.wrapping_add(2);
            }
            call.write_u16(at, 0)?;
        } else {
            call.write(destination, output.as_bytes())?;
            call.write(destination.wrapping_add(output.len() as u32), &[0])?;
        }
    }
    // The count is characters, which for the ANSI form is also bytes.
    Ok(Return(if wide {
        output.encode_utf16().count() as u32
    } else {
        output.len() as u32
    }))
}

/// A window the guest created.
struct Window {
    class: String,
    /// The window procedure the class was registered with. Kept because
    /// `DispatchMessageA` will call it once the pump exists.
    proc_address: u32,
    style: u32,
    /// `left, top, right, bottom`, in the guest's coordinate space.
    rect: [i32; 4],
    /// The caption, or a control's label: what `GetWindowTextA` answers and
    /// `SetWindowTextA` writes.
    text: String,
    /// The control's owner, when the window is one: a dialog's item is a child
    /// of the dialog, which is what `GetParent` and `IsChild` read.
    parent: Option<u32>,
    /// Whether the window is on screen. This host has one visible surface, so a
    /// window is visible when it is shown and the page is looking at it; the
    /// flag exists because `IsWindowVisible` and `ShowWindow` must agree.
    visible: bool,
    /// Whether the window accepts input, which `EnableWindow` moves and
    /// `IsWindowEnabled` reads. A game greys an option out by disabling it.
    enabled: bool,
}

/// One entry of a menu, in the order it is shown.
#[derive(Clone, Debug, Default)]
pub struct MenuItem {
    /// The command id `WM_COMMAND` carries; zero for a separator.
    pub id: u32,
    /// `MF_*` flags: checked, enabled, greyed, the separator bit, and whether
    /// the item opens a submenu.
    pub flags: u32,
    pub text: String,
    /// The submenu this item opens, when it opens one.
    pub submenu: Option<u32>,
}

/// A menu: its items, and the resource it was loaded from.
#[derive(Clone, Debug, Default)]
pub struct Menu {
    pub items: Vec<MenuItem>,
    /// The resource id `LoadMenuA` was asked for; zero for a menu the guest
    /// built itself.
    pub resource: u32,
}

/// One scroll bar's state: the range, the page, and where the thumb is.
///
/// A window has two (`SB_HORZ` and `SB_VERT`), and `SetScrollInfo` may set both
/// at once, so the state is kept as a pair rather than one bar per window.
#[derive(Clone, Copy, Debug, Default)]
pub struct ScrollBar {
    pub min: i32,
    pub max: i32,
    pub page: u32,
    pub position: i32,
    /// Whether the guest gave the bar a range or asked for it to be shown: a bar
    /// with no range has nothing to scroll and is not one a `WM_*SCROLL` could
    /// come from.
    pub live: bool,
}

/// A `SetTimer` timer, which the message pump turns into `WM_TIMER`.
#[derive(Clone, Copy, Debug)]
pub struct WindowTimer {
    /// The id `WM_TIMER` carries: the guest's, or one this host handed out when
    /// the guest asked for zero.
    pub id: u32,
    /// The window the timer belongs to; zero is the thread's own, which is the
    /// same window here because there is one thread that pumps.
    pub window: u32,
    /// How often it fires, in milliseconds. Zero fires once.
    pub interval: u32,
    /// The `TIMERPROC` the guest passed, which it wants called instead of a
    /// `WM_TIMER` being queued.
    pub proc_address: u32,
    /// The guest time it is next due at.
    pub due: u64,
}

/// A hook `SetWindowsHookExA` installed. This host has no other process to hook,
/// so a hook is remembered and `CallNextHookEx` answers as the last one does.
#[derive(Clone, Copy, Debug)]
pub struct Hook {
    pub kind: i32,
    pub thread: u32,
    pub proc_address: u32,
    pub module: u32,
}

/// One control of a dialog, as `CreateDialogIndirectParamA` found it in the
/// template.
#[derive(Clone, Debug)]
pub struct DialogItem {
    /// The handle `GetDlgItem` answers for this id.
    pub handle: u32,
    pub text: String,
    pub enabled: bool,
    pub visible: bool,
}

/// A dialog: the window it created, and the controls its template named.
#[derive(Clone, Debug, Default)]
pub struct Dialog {
    pub window: u32,
    /// The controls, by the id the template gave them, in template order.
    pub items: Vec<(u32, DialogItem)>,
}

/// The caret, when the guest made one visible.
#[derive(Clone, Copy, Debug)]
pub struct Caret {
    pub window: u32,
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub visible: bool,
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
    cursor: (i32, i32),
    cursor_visible: i32,
    /// The cursor handle the guest last selected with `SetCursor`, which is what
    /// the next call answers with as the previous one.
    pub cursor_image: u32,
    /// The screen the port presents, when it named one. `None` means the
    /// conventional VGA mode this emulation assumes.
    screen: Option<(i32, i32)>,
    /// The window holding the mouse capture. A game captures the pointer so that
    /// movement outside its window still arrives, which on this machine it always
    /// does; the value exists so `SetCapture` and `ReleaseCapture` can answer with
    /// the previous holder.
    capture: u32,
    /// Menus the guest loaded or built, by handle.
    pub menus: HashMap<u32, Menu>,
    next_menu: u32,
    /// Window properties by window and name: `SetPropA`'s table, which a game
    /// uses in place of a member variable it has no room for.
    pub props: HashMap<(u32, String), u32>,
    /// Scroll state by window: `SB_HORZ` first, `SB_VERT` second.
    pub scroll: HashMap<u32, [ScrollBar; 2]>,
    /// `SetTimer`'s timers, which the pump turns into `WM_TIMER`.
    pub timers: Vec<WindowTimer>,
    next_timer: u32,
    /// `SetWindowsHookExA`'s hooks, in the order they were installed.
    pub hooks: Vec<Hook>,
    next_hook: u32,
    /// The caret, when the guest made one.
    pub caret: Option<Caret>,
    /// Dialogs the guest created, by the window handle it was given.
    pub dialogs: HashMap<u32, Dialog>,
    next_dialog_item: u32,
    /// The names `RegisterWindowMessageA` handed a number to, and the number.
    pub registered: Vec<(String, u32)>,
    /// The windows that hold the activation, the keyboard focus and the
    /// foreground. One machine, one user: each is a single window, and a guest
    /// that asks gets the one it last set.
    pub active: u32,
    pub focus: u32,
    pub foreground: u32,
    /// `GetMessagePos` and `GetMessageTime`: where and when the message the pump
    /// last handed out was produced.
    pub message_pos: (i32, i32),
    pub message_time: u32,
    /// The window whose creation is part-way through, while `CreateWindowExA`
    /// runs the guest routines that creation needs.
    creating: Option<Creating>,
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
                text: String::new(),
                parent: None,
                visible: style & WS_VISIBLE != 0,
                enabled: true,
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

    /// Whether the guest wants the system pointer shown: Windows' display
    /// counter, which `ShowCursor` moves and only goes negative when the guest
    /// asked for it to be hidden. A game that draws its own cursor has asked,
    /// and the page hides its pointer over the canvas to match.
    pub fn cursor_shown(&self) -> bool {
        self.cursor_visible >= 0
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

    /// The size of a window's client area, which is what a context drawing into
    /// it covers.
    pub fn size(&self, handle: u32) -> Option<(i32, i32)> {
        let window = self.windows.get(&handle)?;
        Some((
            window.rect[2] - window.rect[0],
            window.rect[3] - window.rect[1],
        ))
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

// ---- the state the rest of the module reads -------------------------------

/// The accessors the submodules and the other shims share.
///
/// Every one of these is a question about a handle the guest may have made up,
/// so each answers `None` rather than stopping: Win32 answers zero for a window
/// that does not exist, and a guest that passes a stale handle is the guest's
/// problem, not a fault in this machine.
impl Windows {
    /// Whether a handle names a window this host knows.
    pub fn exists(&self, handle: u32) -> bool {
        handle == DESKTOP_WINDOW || self.windows.contains_key(&handle)
    }

    /// A window's rectangle, as `left, top, right, bottom`.
    pub(crate) fn window_rect(&self, handle: u32) -> Option<[i32; 4]> {
        self.rect(handle)
    }

    /// A window's client area, which on a borderless window is the window.
    pub(crate) fn client_rect(&self, handle: u32) -> Option<[i32; 4]> {
        let rect = self.rect(handle)?;
        Some([0, 0, rect[2] - rect[0], rect[3] - rect[1]])
    }

    /// Moves a window, which `SetWindowPos` and `ScrollWindow` both do.
    pub(crate) fn move_window(&mut self, handle: u32, x: i32, y: i32) {
        if let Some(rect) = self.rect(handle) {
            let (width, height) = (rect[2] - rect[0], rect[3] - rect[1]);
            self.set_rect(handle, [x, y, x.saturating_add(width), y.saturating_add(height)]);
        }
    }

    /// The class a window was created from.
    pub(crate) fn class_of(&self, handle: u32) -> Option<&str> {
        self.windows
            .get(&handle)
            .map(|window| window.class.as_str())
    }

    /// The procedure a class registered, which `CreateDialogIndirectParamA` and
    /// `CallWindowProcA` both need.
    pub(crate) fn class_proc(&self, name: &str) -> Option<u32> {
        self.classes
            .iter()
            .find(|(class, _)| class.eq_ignore_ascii_case(name))
            .map(|(_, proc_address)| *proc_address)
    }

    /// Forgets a class, which is what `UnregisterClassA` does: the next
    /// `CreateWindowExA` of that name then fails, as it does on Windows.
    /// Removes a window, which is what a cancelled creation does: the handle is
    /// never handed to the guest, so nothing may still name it.
    pub(crate) fn destroy(&mut self, handle: u32) -> bool {
        self.windows.remove(&handle).is_some()
    }

    /// Replaces a window's procedure, which is what `SetWindowLongA(hwnd,
    /// GWL_WNDPROC, ...)` does -- and what a framework's creation hook uses to
    /// put its own procedure in front of the class's.
    pub(crate) fn set_proc(&mut self, handle: u32, proc_address: u32) {
        if let Some(window) = self.windows.get_mut(&handle) {
            window.proc_address = proc_address;
        }
    }

    pub(crate) fn unregister_class(&mut self, name: &str) -> bool {
        let before = self.classes.len();
        self.classes
            .retain(|(class, _)| !class.eq_ignore_ascii_case(name));
        before != self.classes.len()
    }

    /// The text a window carries: a caption for the guest's window, the label for
    /// a dialog's control.
    pub(crate) fn text_of(&self, handle: u32) -> Option<&str> {
        self.windows
            .get(&handle)
            .map(|window| window.text.as_str())
    }

    pub(crate) fn set_text(&mut self, handle: u32, text: String) {
        if let Some(window) = self.windows.get_mut(&handle) {
            window.text = text;
        }
    }

    pub(crate) fn is_visible(&self, handle: u32) -> bool {
        self.windows
            .get(&handle)
            .is_some_and(|window| window.visible)
    }

    pub(crate) fn set_visible(&mut self, handle: u32, visible: bool) {
        if let Some(window) = self.windows.get_mut(&handle) {
            window.visible = visible;
        }
    }

    pub(crate) fn is_enabled(&self, handle: u32) -> bool {
        self.windows
            .get(&handle)
            .is_some_and(|window| window.enabled)
    }

    /// Enables or disables a window, answering what it was before, which is what
    /// `EnableWindow` returns.
    pub(crate) fn set_enabled(&mut self, handle: u32, enabled: bool) -> bool {
        let Some(window) = self.windows.get_mut(&handle) else {
            return false;
        };
        std::mem::replace(&mut window.enabled, enabled)
    }

    pub(crate) fn parent_of(&self, handle: u32) -> Option<u32> {
        self.windows.get(&handle).and_then(|window| window.parent)
    }

    pub(crate) fn set_parent(&mut self, handle: u32, parent: u32) {
        if let Some(window) = self.windows.get_mut(&handle) {
            window.parent = Some(parent);
        }
    }

    /// The children of a window, in the order they were created: a dialog's
    /// controls, which `GetDlgItem` and `GetNextDlgTabItem` walk.
    pub(crate) fn children_of(&self, handle: u32) -> Vec<u32> {
        let mut children: Vec<u32> = self
            .windows
            .iter()
            .filter(|(_, window)| window.parent == Some(handle))
            .map(|(child, _)| *child)
            .collect();
        children.sort_unstable();
        children
    }

    /// Creates a window for a submodule, which is the same operation
    /// `CreateWindowExA` does without its `WNDCLASS` lookup.
    pub(crate) fn add_window(
        &mut self,
        class: String,
        proc_address: u32,
        style: u32,
        rect: [i32; 4],
    ) -> u32 {
        self.create(class, proc_address, style, rect)
    }

    /// The extra bytes `SetWindowLongA` stashed with a selector.
    pub(crate) fn extra_of(&self, handle: u32, selector: i32) -> u32 {
        self.extra.get(&(handle, selector)).copied().unwrap_or(0)
    }

    pub(crate) fn set_extra(&mut self, handle: u32, selector: i32, value: u32) {
        self.extra.insert((handle, selector), value);
    }

    /// Files a menu and answers its handle.
    pub(crate) fn add_menu(&mut self, menu: Menu) -> u32 {
        let handle = FIRST_MENU + self.next_menu * 4;
        self.next_menu += 1;
        self.menus.insert(handle, menu);
        handle
    }

    pub(crate) fn menu(&self, handle: u32) -> Option<&Menu> {
        self.menus.get(&handle)
    }

    pub(crate) fn menu_mut(&mut self, handle: u32) -> Option<&mut Menu> {
        self.menus.get_mut(&handle)
    }

    /// The menu bar a window carries, which `SetMenu` sets and `GetMenu` reads.
    /// It is kept in the extra-bytes table under its own selector so one map
    /// holds both kinds of per-window value.
    pub(crate) fn menu_of(&self, handle: u32) -> u32 {
        self.extra_of(handle, GWL_MENU)
    }

    pub(crate) fn set_menu(&mut self, handle: u32, menu: u32) {
        self.set_extra(handle, GWL_MENU, menu);
    }

    /// One of a window's two scroll bars, made on first use: Win32 has a bar per
    /// window whether or not the guest ever set a range.
    pub(crate) fn scroll_bar(&self, handle: u32, bar: usize) -> ScrollBar {
        self.scroll
            .get(&handle)
            .map(|pair| pair[bar.min(1)])
            .unwrap_or_default()
    }

    pub(crate) fn set_scroll_bar(&mut self, handle: u32, bar: usize, state: ScrollBar) {
        self.scroll.entry(handle).or_default()[bar.min(1)] = state;
    }

    /// Installs a timer, answering the id `WM_TIMER` will carry.
    ///
    /// A guest that passes zero asks for any id, which Win32 answers with a
    /// number of its own choosing -- and which the guest then keeps, so the same
    /// number has to keep naming the same timer.
    pub(crate) fn add_timer(
        &mut self,
        window: u32,
        id: u32,
        interval: u32,
        proc_address: u32,
        now: u64,
    ) -> u32 {
        let id = if id != 0 {
            id
        } else {
            // Above the ids a guest chooses for itself, so the two cannot
            // collide: a game numbers its timers from one.
            self.next_timer = self.next_timer.max(0x1000) + 1;
            self.next_timer
        };
        // Setting an id that already exists replaces that timer rather than
        // adding a second one with the same id, as Windows does.
        self.timers.retain(|timer| !(timer.window == window && timer.id == id));
        self.timers.push(WindowTimer {
            id,
            window,
            interval,
            proc_address,
            due: now.saturating_add(u64::from(interval.max(1))),
        });
        id
    }

    pub(crate) fn kill_timer(&mut self, window: u32, id: u32) -> bool {
        let before = self.timers.len();
        self.timers
            .retain(|timer| !(timer.window == window && timer.id == id));
        before != self.timers.len()
    }

    /// The timers due at `now`, each re-armed for its next interval. A timer with
    /// a zero interval is handed out once and forgotten, which is what Windows
    /// does with one.
    pub(crate) fn due_timers(&mut self, now: u64) -> Vec<WindowTimer> {
        let mut due = Vec::new();
        let mut keep = Vec::with_capacity(self.timers.len());
        for mut timer in self.timers.drain(..) {
            if timer.due > now {
                keep.push(timer);
                continue;
            }
            due.push(timer);
            if timer.interval > 0 {
                // Re-armed from the schedule rather than from `now`, so a timer
                // that came due while the guest was busy does not drift later and
                // later.
                timer.due = timer.due.saturating_add(u64::from(timer.interval));
                if timer.due <= now {
                    timer.due = now.saturating_add(u64::from(timer.interval));
                }
                keep.push(timer);
            }
        }
        self.timers = keep;
        due
    }

    /// Installs a hook, answering its handle.
    pub(crate) fn add_hook(&mut self, hook: Hook) -> u32 {
        self.hooks.push(hook);
        self.next_hook += 1;
        FIRST_HOOK + self.next_hook * 4
    }

    pub(crate) fn remove_hook(&mut self, handle: u32) -> bool {
        let Some(index) = (0..self.hooks.len()).find(|index| {
            FIRST_HOOK + (*index as u32 + 1) * 4 == handle
        }) else {
            return false;
        };
        self.hooks.remove(index);
        true
    }

    /// Files a dialog and answers the map of its controls, empty until the caller
    /// fills it from the template.
    pub(crate) fn add_dialog(&mut self, window: u32) -> &mut Dialog {
        self.dialogs.insert(
            window,
            Dialog {
                window,
                items: Vec::new(),
            },
        );
        self.dialogs.get_mut(&window).expect("the dialog was just filed")
    }

    /// Adds a control to a dialog, answering the child handle the guest will see.
    pub(crate) fn add_dialog_item(
        &mut self,
        dialog: u32,
        id: u32,
        text: String,
        template: [i32; 4],
    ) -> u32 {
        let child = self.create("Button".to_string(), 0, 0, template);
        self.set_parent(child, dialog);
        self.set_text(child, text.clone());
        self.next_dialog_item += 1;
        if let Some(entry) = self.dialogs.get_mut(&dialog) {
            entry.items.push((
                id,
                DialogItem {
                    handle: child,
                    text,
                    enabled: true,
                    visible: true,
                },
            ));
        }
        child
    }

    /// A control of a dialog by the id its template gave it.
    pub(crate) fn dialog_item(&self, dialog: u32, id: u32) -> Option<&DialogItem> {
        self.dialogs
            .get(&dialog)?
            .items
            .iter()
            .find(|(item_id, _)| *item_id == id)
            .map(|(_, item)| item)
    }

    pub(crate) fn dialog_item_mut(&mut self, dialog: u32, id: u32) -> Option<&mut DialogItem> {
        self.dialogs
            .get_mut(&dialog)?
            .items
            .iter_mut()
            .find(|(item_id, _)| *item_id == id)
            .map(|(_, item)| item)
    }

    /// The id a dialog's control was created with, which `GetDlgCtrlID` answers.
    pub(crate) fn dialog_item_id(&self, dialog: u32, handle: u32) -> Option<u32> {
        self.dialogs
            .get(&dialog)?
            .items
            .iter()
            .find(|(_, item)| item.handle == handle)
            .map(|(id, _)| *id)
    }

    /// The number `RegisterWindowMessageA` gave a name: one per name, and the
    /// same number for the same name across every call.
    pub(crate) fn register_message(&mut self, name: &str) -> u32 {
        if let Some((_, id)) = self
            .registered
            .iter()
            .find(|(registered, _)| registered.eq_ignore_ascii_case(name))
        {
            return *id;
        }
        // Registered messages live in the range Windows reserves for them.
        let id = 0xC000 + self.registered.len() as u32;
        self.registered.push((name.to_owned(), id));
        id
    }

    /// The window under a screen point, which `WindowFromPoint` answers: the
    /// smallest window that contains it, children before their parent.
    pub(crate) fn window_at(&self, x: i32, y: i32) -> Option<u32> {
        let mut best: Option<(u32, i64)> = None;
        for (handle, window) in &self.windows {
            let [left, top, right, bottom] = window.rect;
            if x < left || y < top || x >= right || y >= bottom {
                continue;
            }
            let area = i64::from(right - left) * i64::from(bottom - top);
            if best.is_none_or(|(_, smallest)| area < smallest) {
                best = Some((*handle, area));
            }
        }
        best.map(|(handle, _)| handle)
    }

    /// The first window the guest created, which is its top-level one.
    pub(crate) fn top_window(&self) -> u32 {
        self.main
    }

    /// The window that owns a handle: the parent of a child, the owner of a
    /// popup, and zero for a top-level window that has neither -- which is what
    /// `GetWindow(hwnd, GW_OWNER)` answers on Windows. A desktop handle here is
    /// not an owner anything can be walked from.
    pub(crate) fn owner_of(&self, handle: u32) -> u32 {
        self.parent_of(handle).unwrap_or(0)
    }
}

/// Queues the `WM_TIMER` messages that are due.
///
/// A timer is a message, not a callback, unless the guest asked for a
/// `TIMERPROC` -- and this host's pump is the guest's own loop, so posting here
/// is what makes `PeekMessageA` see it on the next iteration.
fn deliver_timers(host: &mut Host) {
    let now = host.now();
    let due = host.windows.due_timers(now);
    if due.is_empty() {
        return;
    }
    for timer in due {
        let target = if timer.window != 0 {
            timer.window
        } else {
            host.windows.top_window()
        };
        host.input
            .post(target, WM_TIMER, timer.id, timer.proc_address);
    }
    host.wake_message_waiters();
}

// ---- window classes and windows ------------------------------------------

fn register_class(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    register(host, call, false)
}

/// `RegisterClassW`: the same `WNDCLASSW`, whose name is UTF-16.
fn register_class_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    register(host, call, true)
}

fn register(host: &mut Host, call: &mut Call<'_>, wide: bool) -> Result<Action> {
    let class = call.arg(0)?;
    if class == 0 {
        return Err("RegisterClass without a WNDCLASS".to_string());
    }
    let proc_address = call.memory.read_u32(class.wrapping_add(WNDCLASS_PROC))?;
    let name = call.memory.read_u32(class.wrapping_add(WNDCLASS_NAME))?;

    let name = if wide {
        call.wstr(name)?
    } else {
        call.memory.read_cstr(name)?
    };
    // The atom is not a real atom; it only has to be non-zero and stable.
    Ok(Return(host.windows.register(name, proc_address)))
}

fn create_window_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let class = optional_cstr(call, 1)?;
    let title = optional_cstr(call, 2)?;
    create(host, call, class, title)
}

/// `CreateWindowExW`: the same window, with a UTF-16 class name and title.
fn create_window_ex_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let class = optional_wide(call, 1)?;
    let title = optional_wide(call, 2)?;
    create(host, call, class, title)
}

fn create(host: &mut Host, call: &mut Call<'_>, class: String, title: String) -> Result<Action> {
    // A creation that is part-way through has just come back from one of its
    // guest routines, and the next step runs from here.
    if host.resumed {
        return resume_create(host, call);
    }
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
    let _ = title;
    // Windows runs the creation sequence here, before this import returns: the
    // `WH_CBT` hook, then `WM_NCCREATE`, then `WM_CREATE`. It is not optional for
    // a window an application creates through a framework -- MFC registers its
    // classes with `DefWindowProcA` and uses the hook to swap in its own
    // procedure -- and a host that skips it hands the guest a window that exists
    // and is never shown.
    begin_create(host, call, handle, style, [x, y, width, height])
}

/// One step of a window's creation, so a resumed shim knows what just returned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CreationStep {
    /// The `WH_CBT` hook, called with `HCBT_CREATEWND`.
    Hook,
    /// `WM_NCCREATE`, whose zero answer cancels the creation.
    NcCreate,
    /// `WM_CREATE`, whose -1 answer cancels the creation.
    Create,
}

/// A window whose creation is between two guest routines.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Creating {
    window: u32,
    step: CreationStep,
    style: u32,
    /// The `CREATESTRUCT` in guest memory, which both messages carry as `lparam`.
    lparam: u32,
}

/// Whether a value points at guest code rather than at one of this host's own
/// import stubs.
///
/// This is the answer to a question that looked like a broken binary: a class
/// registered through an imported `DefWindowProcA` holds the *stub address* the
/// import table was filled with, not a function in the image. Calling it would
/// run the import stub as a window procedure, so every step below treats it as
/// what it is -- the default procedure, which answers zero.
fn is_guest_code(host: &Host, entry: u32) -> bool {
    entry != 0 && crate::cpu::host_slot_at(entry, host.target.table).is_none()
}

/// One line of the creation sequence, under `SHIMKIT_LOG_WINDOWS`: which guest
/// routine ran and what it answered, which is what a window that never appears
/// is diagnosed from.
fn trace_creation(host: &mut Host, line: String) {
    if std::env::var("SHIMKIT_LOG_WINDOWS").is_ok() {
        host.notice(format!("create: {line}"));
    }
}

/// Fills the window's `CREATESTRUCT` and starts its creation sequence.
fn begin_create(
    host: &mut Host,
    call: &mut Call<'_>,
    handle: u32,
    style: u32,
    size: [i32; 4],
) -> Result<Action> {
    let lparam = host.allocate(CREATESTRUCT_BYTES, 4)?;
    call.zero(lparam, CREATESTRUCT_BYTES)?;
    // `hInstance`, the size and place, and the style are the members a guest's
    // `WM_CREATE` reads. The class and window names stay null: a window created
    // from a registered class name carries none, which is what Windows does.
    for (offset, value) in [
        (4_u32, crate::pe::IMAGE_BASE),
        (16, size[3] as u32),
        (20, size[2] as u32),
        (24, size[1] as u32),
        (28, size[0] as u32),
        (32, style),
    ] {
        call.memory.write_u32(lparam.wrapping_add(offset), value)?;
    }
    let creating = Creating {
        window: handle,
        step: CreationStep::Hook,
        style,
        lparam,
    };
    host.windows.creating = Some(creating);
    run_hook(host, call, creating)
}

/// The `WH_CBT` hook: the last one installed, called with `HCBT_CREATEWND`.
///
/// Its return value is not read. A CBT hook exists for what it does to the
/// window -- a framework attaches its object and replaces the procedure -- and
/// `FALSE`, which every one of them returns, means "go on and create it".
fn run_hook(host: &mut Host, call: &mut Call<'_>, creating: Creating) -> Result<Action> {
    let Some(entry) = host
        .windows
        .hooks
        .iter()
        .rev()
        .find(|hook| hook.kind == WH_CBT && hook.proc_address != 0)
        .map(|hook| hook.proc_address)
    else {
        return send_nc_create(host, call, creating);
    };
    // `CBT_CREATEWND { CREATESTRUCT *lpcs; HWND hwndInsertAfter; }`
    let address = host.allocate(8, 4)?;
    call.memory.write_u32(address, creating.lparam)?;
    call.memory.write_u32(address.wrapping_add(4), 0)?;
    host.windows.creating = Some(Creating { step: CreationStep::Hook, ..creating });
    trace_creation(
        host,
        format!("{:#010x}: WH_CBT hook {entry:#010x}", creating.window),
    );
    host.guest_call = Some(GuestCall {
        entry,
        args: [HCBT_CREATEWND, creating.window, address, 0],
        answer: None,
        resume: true,
    });
    Ok(Return(creating.window))
}

/// `WM_NCCREATE`, then `WM_CREATE`, sent to whatever procedure the window has
/// now: the hook above is what gives it the one that handles them.
fn send_nc_create(host: &mut Host, call: &mut Call<'_>, creating: Creating) -> Result<Action> {
    send_creation(host, call, Creating { step: CreationStep::NcCreate, ..creating }, WM_NCCREATE)
}

fn send_create(host: &mut Host, call: &mut Call<'_>, creating: Creating) -> Result<Action> {
    send_creation(host, call, Creating { step: CreationStep::Create, ..creating }, WM_CREATE)
}

fn send_creation(
    host: &mut Host,
    _call: &mut Call<'_>,
    creating: Creating,
    message: u32,
) -> Result<Action> {
    let entry = host.windows.proc_address(creating.window).unwrap_or(0);
    if !is_guest_code(host, entry) {
        // A class that still holds the default procedure: nothing to call, and
        // nothing to wait for, so the next message goes out now.
        return if message == WM_NCCREATE {
            send_create(host, _call, creating)
        } else {
            end_create(host, creating)
        };
    }
    trace_creation(
        host,
        format!(
            "{:#010x}: {message:#06x} to {entry:#010x}",
            creating.window
        ),
    );
    host.windows.creating = Some(creating);
    host.guest_call = Some(GuestCall {
        entry,
        args: [creating.window, message, 0, creating.lparam],
        answer: None,
        resume: true,
    });
    Ok(Return(creating.window))
}

/// The sequence is over: a window created with `WS_VISIBLE` is on screen from the
/// moment it exists, so it is activated now, exactly as Windows activates it.
fn end_create(host: &mut Host, creating: Creating) -> Result<Action> {
    let proc_address = host.windows.proc_address(creating.window).unwrap_or(0);
    trace_creation(
        host,
        format!(
            "{:#010x}: created, proc {proc_address:#010x}",
            creating.window
        ),
    );
    host.windows.creating = None;
    if creating.style & WS_VISIBLE != 0 {
        activate(host, creating.window);
    }
    Ok(Return(creating.window))
}

/// The step a guest routine just finished, for a `CreateWindowExA` that asked to
/// be resumed.
fn resume_create(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let creating = host
        .windows
        .creating
        .expect("a resumed creation has a window");
    let answer = host.guest_result;
    match creating.step {
        // The hook's own value says nothing this host needs.
        CreationStep::Hook => send_nc_create(host, call, creating),
        // `WM_NCCREATE` answering zero cancels the window, and Windows then
        // answers `CreateWindowExA` with NULL.
        CreationStep::NcCreate if answer == 0 => {
            trace_creation(
                host,
                format!("{:#010x}: WM_NCCREATE refused", creating.window),
            );
            destroy_created(host, creating)
        }
        CreationStep::NcCreate => {
            trace_creation(
                host,
                format!("{:#010x}: WM_NCCREATE answered", creating.window),
            );
            send_create(host, call, creating)
        }
        // `WM_CREATE` answering -1 abandons it the same way.
        CreationStep::Create if answer == u32::MAX => {
            trace_creation(host, format!("{:#010x}: WM_CREATE refused", creating.window));
            destroy_created(host, creating)
        }
        CreationStep::Create => end_create(host, creating),
    }
}

fn destroy_created(host: &mut Host, creating: Creating) -> Result<Action> {
    host.windows.creating = None;
    host.windows.destroy(creating.window);
    Ok(Return(0))
}

/// Queues the messages a window receives when it becomes the one the user is
/// looking at, once per session.
///
/// `pub(crate)` because a full-screen Direct3D device takes the display over,
/// and the runtime showing the window is how a full-screen game learns it has
/// the user's attention.
pub(crate) fn activate(host: &mut Host, handle: u32) {
    if host.windows.activated || handle == 0 {
        return;
    }
    host.windows.activated = true;
    host.input.post(handle, WM_ACTIVATE, WA_ACTIVE, 0);
    host.input.post(handle, WM_SETFOCUS, 0, 0);
    host.input.post(handle, WM_ACTIVATEAPP, 1, 0);
    host.wake_message_waiters();
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
    // `SW_HIDE` (0) is the one command that takes the window off the screen; the
    // rest ask for a state a borderless window is already in.
    if call.arg(1)? != 0 {
        host.windows.set_visible(handle, true);
        // A window that has just been shown is owed a paint, exactly as on
        // Windows: the guest's menu is drawn in `WM_PAINT`, so a host that never
        // queues one shows an empty frame.
        request_paint(host, handle);
    } else {
        host.windows.set_visible(handle, false);
    }
    activate(host, handle);
    Ok(Return(1))
}

/// `AFX_IDS_APP_TITLE`: the first string MFC asks for, from its own reserved
/// range. A statically linked MFC program carries it only when its `.rc` included
/// `afxres.rc`; when it does not, `CWinApp` falls back to the executable's name.
const AFX_IDS_APP_TITLE: u32 = 0xE000;

/// The title an MFC program with no app-title string of its own presents: the
/// executable's name without its extension, which is exactly what MFC's fallback
/// in `CWinApp` produces (`HERCULES.EXE` becomes `HERCULES`).
fn app_title(exe_name: &str) -> &str {
    match exe_name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => stem,
        _ => exe_name,
    }
}

/// The resource tree an instance handle names, the base its RVAs are relative to,
/// and a name for it to say in a log.
///
/// A guest passes a module handle to `LoadStringA` and every other `Load*` call,
/// and the answer comes from that module's own `.rsrc`. `0` and the program's own
/// base both mean the program, which is the handle a program that has no DLL of
/// its own passes.
pub(crate) fn instance_resources(
    host: &Host,
    instance: u32,
) -> (&[crate::pe::Resource], u32, &str) {
    if instance == 0 || instance == crate::pe::IMAGE_BASE {
        return (&host.resources, crate::pe::IMAGE_BASE, "the image");
    }
    match host
        .guest_modules
        .iter()
        .find(|module| module.base == instance)
    {
        Some(module) => (&module.resources, module.base, module.name.as_str()),
        // A handle nothing was mapped at: the guest asked the wrong question, and
        // answering from the program's tree would hide that.
        None => (&[], 0, "an unknown module"),
    }
}

/// Queues the paint a window is owed, once.
///
/// `WM_PAINT` is posted rather than sent -- the guest draws when its pump reaches
/// it -- and it coalesces: however often a window is invalidated, one paint waits
/// until the guest takes it. Without this, a title whose window only draws in
/// `WM_PAINT` (an MFC launcher, a dialog) has nothing to draw with, which looks
/// exactly like a game that started and never painted.
fn request_paint(host: &mut Host, handle: u32) {
    if handle == 0 || host.input.waiting(handle, WM_PAINT) {
        return;
    }
    host.input.post(handle, WM_PAINT, 0, 0);
    host.wake_message_waiters();
}

fn update_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Windows paints the window now: `UpdateWindow` sends `WM_PAINT` before it
    // returns, which is where a guest that draws on demand does its first frame.
    let handle = call.arg(0)?;
    if handle != 0 {
        host.input.post(handle, WM_PAINT, 0, 0);
        host.wake_message_waiters();
    }
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

    deliver_timers(host);
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

/// `GetMessageA` cannot block: the page cannot interrupt a call, and the
/// guest's own window procedure is what drives its frames -- a message answered
/// per pump iteration is a tick the game runs on. An empty queue therefore
/// answers `WM_NULL`, which every window ignores, and the next event finds the
/// guest still in its loop.
fn get_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // `GetMessageA(lpMsg, hwnd, filterMin, filterMax)`, same order as the peek.
    let destination = call.arg(0)?;
    let window = call.arg(1)?;
    let min = call.arg(2)?;
    let max = call.arg(3)?;

    deliver_timers(host);
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
    if std::env::var("SHIMKIT_LOG_USER").is_ok() && host.notices.len() < 600 {
        host.notice(format!(
            "DispatchMessage {message:#06x} to {window:#x} wparam {wparam:#x} lparam {lparam:#x}"
        ));
    }
    if window == 0 {
        // `WM_QUIT` from a thread queue: there is no window and no procedure.
        return Ok(Return(0));
    }
    match host.windows.proc_address(window) {
        // A class that registered an imported `DefWindowProcA` holds that
        // import's stub address, not code: dispatching would run the stub.
        Some(entry) if !is_guest_code(host, entry) => Ok(Return(0)),
        Some(entry) => {
            host.guest_call = Some(GuestCall {
                entry,
                args: [window, message, wparam, lparam],
                // The procedure's return value finishes the import; nothing else
                // of this shim is left to do.
                answer: None,
                resume: false,
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

/// `GetForegroundWindow()`: the window the visitor is looking at, which here is
/// the one the page's input belongs to.
fn get_foreground_window(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.windows.main_window()))
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

fn optional_wide(call: &Call<'_>, index: u32) -> Result<String> {
    call.wstr(call.arg(index)?)
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
        // `GWL_WNDPROC` is the one selector that changes where messages go, so
        // it is stored where dispatch reads it rather than in the extra table.
        GWL_WNDPROC => host.windows.set_proc(handle, value),
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
        GWL_WNDPROC => host.windows.proc_address(handle).unwrap_or(0),
        _ => host
            .windows
            .extra
            .get(&(handle, selector))
            .copied()
            .unwrap_or(0),
    }
}

fn get_menu(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The game draws its own screens, so it rarely asks; a guest that *did* load
    // a menu and install it gets the bar it set, which is what makes
    // `EnableMenuItem(GetMenu(hwnd), ...)` act on the menu the player sees rather
    // than on handle zero.
    Ok(Return(host.windows.menu_of(call.arg(0)?)))
}

fn def_window_proc(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Nothing here has a title bar, border or menu for the default procedure to
    // draw or answer for; the guest's own drawing is the window.
    //
    // `WM_NCCREATE` is the exception, and not a detail: the default procedure is
    // what says a window may be created, so it answers TRUE. A framework that
    // routes `WM_NCCREATE` through `DefWindowProc` -- MFC does, from `AfxWndProc`
    // when the window is not one of its permanent objects yet -- would otherwise
    // be told the creation failed, and destroy the frame it just made.
    let message = call.arg(1)?;
    Ok(Return(u32::from(message == WM_NCCREATE)))
}

/// `GetClientRect(hwnd, &rect)`: the window's own area, from the origin.
///
/// There is no border and no caption, so the client area is the window's size.
fn get_client_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let destination = call.arg(1)?;
    let Some(rect) = host.windows.rect(handle) else {
        return Ok(Return(0));
    };
    if destination != 0 {
        write_rect_at(
            call,
            destination,
            [0, 0, rect[2] - rect[0], rect[3] - rect[1]],
        )?;
    }
    Ok(Return(1))
}

/// `PtInRect(rect, point)`: Windows' half-open test, in which the left and top
/// edges belong to the rectangle and the right and bottom do not -- so a point on
/// a shared edge is inside exactly one of two adjacent rectangles.
fn pt_in_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let rect = call.arg(0)?;
    let x = call.arg(1)? as i32;
    let y = call.arg(2)? as i32;
    if rect == 0 {
        return Ok(Return(0));
    }
    let left = call.memory.read_u32(rect)? as i32;
    let top = call.memory.read_u32(rect.wrapping_add(4))? as i32;
    let right = call.memory.read_u32(rect.wrapping_add(8))? as i32;
    let bottom = call.memory.read_u32(rect.wrapping_add(12))? as i32;
    Ok(Return(u32::from(
        left <= x && x < right && top <= y && y < bottom,
    )))
}

/// `ClientToScreen(hwnd, &point)`: the client's origin added to the point.
fn client_to_screen(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let point = call.arg(1)?;
    let Some(rect) = host.windows.rect(handle) else {
        return Ok(Return(0));
    };
    if point == 0 {
        return Ok(Return(0));
    }
    let x = call.memory.read_u32(point)? as i32 + rect[0];
    let y = call.memory.read_u32(point.wrapping_add(4))? as i32 + rect[1];
    call.write_u32(point, x as u32)?;
    call.write_u32(point.wrapping_add(4), y as u32)?;
    Ok(Return(1))
}

/// `InvalidateRect(hwnd, &rect, erase)`: the window is owed a paint.
///
/// There is no update region here -- a paint is the guest drawing its whole frame,
/// and `BeginPaint` says `rcPaint` is the client area -- so all that is left of the
/// call is the message Windows posts from it.
fn invalidate_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    request_paint(host, window);
    Ok(Return(1))
}

/// `GetKeyState(vk)`: the key's state as the queue sees it.
///
/// Bit 15 is down, the toggle bit is never set because this host does not track
/// one, and -- unlike `GetAsyncKeyState` -- the reading does not consume the
/// press edge, which is the difference the two functions exist for.
fn get_key_state(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let key = call.arg(0)? as u8;
    Ok(Return(u32::from(host.input.keyboard_state(key))))
}

/// `MapVirtualKeyA(code, map)`: zero, because this machine has no scan-code
/// table. The browser reports which key it was, not where it sat on a board, so
/// a translation to a scan code or a virtual key has no honest answer.
fn map_virtual_key(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let _ = call.arg(0)?;
    Ok(Return(0))
}

/// `SetCursor(cursor)`: the cursor that was set before it.
///
/// The page keeps the pointer the visitor sees, so this records the guest's
/// choice and nothing on screen changes; what a game reads back is the previous
/// handle, as on Windows.
fn set_cursor(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let cursor = call.arg(0)?;
    Ok(Return(std::mem::replace(
        &mut host.windows.cursor_image,
        cursor,
    )))
}

// ---- device contexts, cursor and input -----------------------------------

/// `GetDC(hwnd)` and `GetWindowDC(hwnd)`: the context that draws on the screen.
///
/// It is GDI's own context, not a number of its own, so the guest can select a
/// font or a brush into it and `BitBlt` a finished menu through it -- and what
/// lands is the primary surface the page shows.
fn get_dc(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // A guest that draws its menus with GDI has no DirectDraw surface behind them,
    // so the screen is made here: the first `GetDC` is the first moment anything
    // knows how large the guest's window is.
    crate::shim::gdi32::ensure_window_screen(host)?;
    Ok(Return(host.gdi.window_dc()))
}

fn release_dc(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn show_cursor(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Windows keeps a display counter that hides the pointer at zero; the same
    // arithmetic here means a `ShowCursor(FALSE)` from the game hides the page's
    // own pointer too once input is wired to it.
    let show = call.arg(0)?;
    let before = host.windows.cursor_shown();
    host.windows.cursor_visible += if show != 0 { 1 } else { -1 };
    // The page keeps the real pointer, so this only tells the loader what the
    // guest decided: one counter, one cursor, as on Windows. Worth saying once
    // per change -- a game that hides the pointer draws its own, and a page that
    // keeps showing one leaves two cursors for one click.
    if before != host.windows.cursor_shown() {
        host.notice(format!(
            "ShowCursor({}) -> {}",
            show,
            if host.windows.cursor_shown() {
                "shown"
            } else {
                "hidden; the guest draws its own"
            }
        ));
    }
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

/// `LoadImageA(instance, name, type, cx, cy, flags)`.
///
/// Icons and cursors answer like `LoadIconA` and `LoadCursorA`: this machine has
/// the predefined ones and nothing else. A bitmap comes either from the image's
/// own resource tree -- `RT_BITMAP` is type 2 -- or, under `LR_LOADFROMFILE`,
/// from a file in the file table; the name is a path in that case, and an image
/// that has neither answers 0, which is what Windows does for a name it cannot
/// find.
///
/// Cave Story loads every `data/*.pbm` this way: they are ordinary `.bmp` files
/// with a different extension, and it hands the handle to `GetObjectA` and then
/// to a `StretchBlt` that puts it on a DirectDraw surface.
fn load_image(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(1)?;
    let kind = call.arg(2)?;
    let flags = call.arg(5)?;
    if std::env::var("SHIMKIT_LOG_IMAGES").is_ok() && host.notices.len() < 400 {
        let path = call.memory.read_cstr(name).unwrap_or_default();
        host.notice(format!("LoadImageA name {path:?} type {kind} flags {flags:#x}"));
    }
    match kind {
        IMAGE_ICON => Ok(Return(predefined(name, FIRST_ICON_ID, APPLICATION_ICON))),
        IMAGE_CURSOR => Ok(Return(predefined(name, FIRST_CURSOR_ID, ARROW_CURSOR))),
        IMAGE_BITMAP if flags & LR_LOADFROMFILE != 0 => {
            let path = call.memory.read_cstr(name)?;
            let log = std::env::var("SHIMKIT_LOG_IMAGES").is_ok() && host.notices.len() < 400;
            let Some(bytes) = host.files.contents(&path).map(<[u8]>::to_vec) else {
                // The file table is the only filesystem here, and the missing
                // name is the answer a caller that has a fallback already
                // handles -- Cave Story treats it as a failed load.
                if log {
                    host.notice(format!("LoadImageA: missing {path}"));
                }
                return Ok(Return(0));
            };
            if log {
                host.notice(format!("LoadImageA: {path} ({} bytes)", bytes.len()));
            }
            let base = host.allocate(bytes.len() as u32, 4)?;
            call.memory.write_bytes(base, &bytes)?;
            Ok(Return(host.gdi.add_bmp(call.memory, base)?))
        }
        IMAGE_BITMAP => {
            let key = if name < 0x1_0000 {
                crate::pe::ResourceKey::Id(name)
            } else {
                crate::pe::ResourceKey::Name(call.memory.read_cstr(name)?)
            };
            let found = host.resources.iter().position(|resource| {
                resource.kind == crate::pe::ResourceKey::Id(RT_BITMAP)
                    && resource.name == key
            });
            let Some(index) = found else {
                return Ok(Return(0));
            };
            let handle = FIRST_BITMAP + index as u32 * 4;
            // The handle is also the GDI object: a guest selects it into a DC
            // and reads its size back with `GetObjectA`.
            let rva = host.resources[index].rva;
            host.gdi
                .add_bitmap(call.memory, handle, crate::pe::IMAGE_BASE.wrapping_add(rva))?;
            Ok(Return(handle))
        }
        _ => Ok(Return(0)),
    }
}

/// `SetCapture(window)`: takes the mouse capture and answers its previous holder.
fn set_capture(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let previous = host.windows.capture;
    host.windows.capture = window;
    Ok(Return(previous))
}

/// `ReleaseCapture()`: gives the capture up.
fn release_capture(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let previous = host.windows.capture;
    host.windows.capture = 0;
    Ok(Return(previous))
}

/// `RT_STRING`, the resource type Windows files text under.
const RT_STRING: u32 = 6;

/// `LoadStringA(instance, id, buffer, max)`.
///
/// A string resource is not one resource per string. Windows files them in blocks
/// of sixteen, under the resource name `(id >> 4) + 1`, each string preceded by a
/// 16-bit count of UTF-16 code units. So this reads the block and walks it, and a
/// block or entry that is not there is an empty string -- which is what a program
/// that falls back to a default already handles.
///
/// `instance` picks the table: a title that keeps its text in a DLL asks for that
/// DLL's handle, and MFC asks for the module carrying its own strings. Reading the
/// program's tree whatever was asked answers with another module's text, or with
/// nothing at all, which is a miss the guest cannot tell from a block it never
/// shipped.
///
/// The one string that is answered without a resource is MFC's own app title: see
/// [`AFX_IDS_APP_TITLE`].
fn load_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let instance = call.arg(0)?;
    let id = call.arg(1)?;
    let buffer = call.arg(2)?;
    let limit = call.arg(3)?;
    if buffer == 0 {
        return Ok(Return(0));
    }
    let block = crate::pe::ResourceKey::Id((id >> 4) + 1);
    let found = {
        let (resources, base, _) = instance_resources(host, instance);
        resources
            .iter()
            .find(|resource| {
                resource.kind == crate::pe::ResourceKey::Id(RT_STRING) && resource.name == block
            })
            .map(|resource| (base.wrapping_add(resource.rva), resource.size))
    };
    let Some((at, size)) = found else {
        // MFC's app-title string is the one id answered without a resource: a
        // statically linked build carries it only when its `.rc` included
        // `afxres.rc`, and MFC's own fallback when it is missing is the
        // executable's name -- so that is the title the guest gets, not a gap.
        if id == AFX_IDS_APP_TITLE {
            let title = app_title(host.target.exe_name);
            let room = (limit as usize).saturating_sub(1);
            let mut text = title.as_bytes().to_vec();
            text.truncate(room);
            let copied = text.len() as u32;
            text.push(0);
            call.write(buffer, &text)?;
            return Ok(Return(copied));
        }
        // A string the module does not carry is an empty string, which is what
        // Windows answers too: every caller of this has a fallback, and saying so
        // in the log only made a normal miss look like a broken image.
        call.write(buffer, &[0])?;
        return Ok(Return(0));
    };
    let bytes = call.memory.read_bytes(at, size as usize)?.to_vec();

    let mut offset = 0_usize;
    let mut index = 0_u32;
    while offset + 2 <= bytes.len() {
        let length = usize::from(u16::from_le_bytes([bytes[offset], bytes[offset + 1]]));
        let start = offset + 2;
        let end = start + length * 2;
        if end > bytes.len() {
            break;
        }
        if index == id & 0xf {
            // `LoadStringA` answers in the ANSI code page. These are the image's
            // own strings: one byte each covers what they carry, and anything
            // wider becomes `?` rather than a byte of the wrong character.
            let mut text: Vec<u8> = bytes[start..end]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .map(|unit| u8::try_from(unit).unwrap_or(b'?'))
                .collect();
            // A zero limit asks for the length only, and the buffer keeps its
            // slot for the terminator.
            let room = (limit as usize).saturating_sub(1);
            text.truncate(room);
            let copied = text.len() as u32;
            text.push(0);
            call.write(buffer, &text)?;
            return Ok(Return(copied));
        }
        offset = end;
        index += 1;
    }
    call.write(buffer, &[0])?;
    Ok(Return(0))
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

/// `MessageBoxW(owner, text, caption, type)`: the same notice, with UTF-16 text.
///
/// A box does not block here, so the answer is the button the guest would have
/// got from a player who pressed it: the first one, which is what a program that
/// only reports a failure does with the result.
fn message_box_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = call.wstr(call.arg(1)?)?;
    let caption = call.wstr(call.arg(2)?)?;
    host.notice(format!("MessageBoxW \"{caption}\": {text}"));
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

    /// The width modifier decides how wide an argument string is, not the
    /// format: `%hs` in a wide format names an ANSI string. Reading it as UTF-16
    /// turned the save path into mojibake, and the guest then asked the
    /// filesystem for a directory that cannot exist.
    #[test]
    fn a_format_takes_the_string_width_from_its_modifier() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let mut format = Vec::new();
        for unit in "%hs".encode_utf16() {
            format.extend_from_slice(&unit.to_le_bytes());
        }
        format.extend_from_slice(&[0, 0]);
        let format_at = STACK_BASE + 0x3700;
        memory.write_bytes(format_at, &format).unwrap();
        let path = STACK_BASE + 0x3740;
        memory.write_bytes(path, b"C:\\Save\\\0").unwrap();

        let destination = STACK_BASE + 0x3780;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "wsprintfW",
                &[destination, format_at, path, 0, 0, 0, 0]
            )
            .unwrap(),
            Return(8),
            "eight characters, not the four a UTF-16 read would find"
        );
        let mut wide = Vec::new();
        for index in 0..8 {
            wide.push(memory.read_u16(destination + index * 2).unwrap());
        }
        assert_eq!(String::from_utf16(&wide).unwrap(), "C:\\Save\\");
        assert_eq!(memory.read_u16(destination + 16).unwrap(), 0);

        // And the other way round: `%ls` in an ANSI format names a wide string.
        memory.write_bytes(format_at, b"%ls\0").unwrap();
        let wide_path = STACK_BASE + 0x37c0;
        for (index, unit) in "Hi".encode_utf16().chain([0]).enumerate() {
            memory.write_u16(wide_path + index as u32 * 2, unit).unwrap();
        }
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "wsprintfA",
                &[destination, format_at, wide_path, 0, 0, 0, 0]
            )
            .unwrap(),
            Return(2)
        );
        assert_eq!(memory.read_bytes(destination, 3).unwrap(), b"Hi\0");
    }

    #[test]
    fn the_keyboard_and_the_character_walk_are_the_ones_a_pc_reports() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetKeyboardType", &[0]).unwrap(),
            Return(1),
            "an enhanced 101-key PC"
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetKeyboardType", &[2]).unwrap(),
            Return(12),
            "twelve function keys"
        );

        let text = STACK_BASE + 0x2600;
        memory.write_bytes(text, b"ab\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "CharNextA", &[text]).unwrap(),
            Return(text + 1)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "CharNextA", &[text + 1]).unwrap(),
            Return(text + 2)
        );
        // At the terminator the walk stops rather than stepping past the string.
        assert_eq!(
            call_shim(&mut host, &mut memory, "CharNextA", &[text + 2]).unwrap(),
            Return(text + 2)
        );
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
    /// MFC asks for its own app-title string before it does anything, and a build
    /// that never linked `afxres.rc` does not carry it. The answer is the name MFC
    /// would fall back to, so the guest's title is the same as on Windows and the
    /// call succeeds instead of reporting a hole in the image.
    #[test]
    fn mfc_app_title_is_answered_with_the_executable_name() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let buffer = STACK_BASE + 0x200;
        assert_eq!(
            call_shim(&mut host, &mut memory, "LoadStringA", &[0, 0xE000, buffer, 32]).unwrap(),
            Return(7)
        );
        // The reference machine runs `program.exe`, whose title is `program`.
        assert_eq!(memory.read_bytes(buffer, 8).unwrap(), b"program\0");

        // An id beside it is still an ordinary miss: only the title has a value
        // this host can answer without the image carrying one.
        assert_eq!(
            call_shim(&mut host, &mut memory, "LoadStringA", &[0, 0xE001, buffer, 32]).unwrap(),
            Return(0)
        );
    }

    /// The handle a guest passes to `LoadStringA` chooses the table: a module's
    /// own strings come from that module, and asking the program instead does not
    /// find them.
    #[test]
    fn a_string_comes_from_the_module_the_handle_names() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let base = STACK_BASE;
        // Block 3 covers ids 32..47; entry 1 is `"Hi"`, two code units long.
        let mut block = Vec::new();
        block.extend_from_slice(&0_u16.to_le_bytes());
        block.extend_from_slice(&2_u16.to_le_bytes());
        for unit in "Hi".encode_utf16() {
            block.extend_from_slice(&unit.to_le_bytes());
        }
        memory.write_bytes(base, &block).unwrap();
        host.guest_modules.push(crate::shim::GuestModule {
            name: "strings.dll".to_string(),
            base,
            exports: Vec::new(),
            resources: vec![crate::pe::Resource {
                kind: crate::pe::ResourceKey::Id(RT_STRING),
                name: crate::pe::ResourceKey::Id(3),
                rva: 0,
                size: block.len() as u32,
            }],
        });

        let buffer = STACK_BASE + 0x100;
        assert_eq!(
            call_shim(&mut host, &mut memory, "LoadStringA", &[base, 33, buffer, 16]).unwrap(),
            Return(2)
        );
        assert_eq!(memory.read_bytes(buffer, 3).unwrap(), b"Hi\0");

        // The same id asked of the program itself is a block it does not have.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "LoadStringA",
                &[crate::pe::IMAGE_BASE, 33, buffer, 16]
            )
            .unwrap(),
            Return(0)
        );
    }

    /// A window's menu is drawn in `WM_PAINT`, so a window that is shown and an
    /// area that is invalidated each owe one paint -- one, not one per call.
    #[test]
    fn a_window_that_is_shown_or_invalidated_is_owed_one_paint() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        assert!(!host.input.waiting(window, WM_PAINT));

        // SW_SHOW: the window is on screen, so the guest's own paint runs.
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowWindow", &[window, 5]).unwrap(),
            Return(1)
        );
        assert!(
            host.input.waiting(window, WM_PAINT),
            "showing a window posts the paint that draws it"
        );

        // Invalidated twice while that paint waits: still one, which is what keeps
        // a guest that invalidates inside its paint handler from never leaving it.
        call_shim(&mut host, &mut memory, "InvalidateRect", &[window, 0, 0]).unwrap();
        call_shim(&mut host, &mut memory, "InvalidateRect", &[window, 0, 0]).unwrap();
        let first = host
            .input
            .find(window, WM_PAINT, WM_PAINT)
            .expect("the paint is still queued");
        host.input.remove(first).unwrap();
        assert!(
            host.input.find(window, WM_PAINT, WM_PAINT).is_none(),
            "three invalidations queue one paint"
        );

        // Hiding is not painting.
        call_shim(&mut host, &mut memory, "ShowWindow", &[window, 0]).unwrap();
        assert!(!host.input.waiting(window, WM_PAINT));
    }

    fn get_message_keeps_the_guest_alive_when_nothing_is_queued() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_with_proc(&mut host, &mut memory, 0x0045_7da3);
        let message = STACK_BASE + 0x3f00;
        // An empty queue answers `WM_NULL`, which keeps the guest's own loop
        // alive instead of ending the process on a message that never came.
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
                answer: None,
                resume: false,
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
        assert!(host.windows.cursor_shown(), "a machine starts with a pointer");
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowCursor", &[0]).unwrap(),
            Return((-1_i32) as u32)
        );
        // The guest asked, so the page hides its pointer: the guest draws one.
        assert!(!host.windows.cursor_shown());
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowCursor", &[1]).unwrap(),
            Return(0)
        );
        assert!(host.windows.cursor_shown());
        assert_eq!(
            call_shim(&mut host, &mut memory, "ShowCursor", &[1]).unwrap(),
            Return(1)
        );
        assert!(host.windows.cursor_shown());
    }

    #[test]
    fn a_pointer_placed_on_the_screen_reports_that_position() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let _ = &mut memory;
        host.pointer(219, 351);
        assert_eq!(host.windows.cursor(), (219, 351));
        // The guest gets the step from the cursor it is tracking, so asking for
        // the same place twice is not another 219 pixels of movement.
        let _ = host.input.take_mouse_motion();
        host.pointer(219, 351);
        assert_eq!(host.input.take_mouse_motion(), (0, 0));
        host.pointer(120, 40);
        assert_eq!(host.input.take_mouse_motion(), (-99, -311));
    }

}
