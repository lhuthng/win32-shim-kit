//! The `USER32.dll` calls that ask a window about itself: its class, its text,
//! its owner, its placement, and which window a screen point lands in.
//!
//! A game asks these while it arranges its own screen. It reads the rectangle of
//! a control it is about to draw over, asks `IsWindowVisible` before drawing a
//! window it only sometimes shows, and walks its children to lay a dialog out.
//! None of that paints anything, but a wrong answer moves the game's own drawing,
//! so the answers describe the windows the guest actually created rather than
//! assuming the one this host expects.
//!
//! Three of these calls change what the guest's next call sees: the activation,
//! focus and foreground windows. This host has no window manager -- the page *is*
//! the window -- so each is a single handle, recorded when the guest sets it and
//! answered back when the guest asks, falling back to the guest's own window
//! because that is the one the page's input reaches.
//!
//! `CallWindowProcA` is the one call here that runs guest code. It leaves a
//! [`GuestCall`] for the session, exactly as `DispatchMessageA` does, because a
//! shim cannot call the guest while the guest's frame is suspended underneath it.

use super::{DESKTOP_WINDOW, WNDCLASS_NAME, WNDCLASS_PROC};
use crate::shim::{Action, Binding, Call, GuestCall, Host};
use crate::Result;

use Action::Return;

const USER32: &str = "USER32.dll";

/// `GetWindow`'s selectors.
const GW_HWNDFIRST: u32 = 0;
const GW_HWNDLAST: u32 = 1;
const GW_HWNDNEXT: u32 = 2;
const GW_HWNDPREV: u32 = 3;
const GW_OWNER: u32 = 4;
const GW_CHILD: u32 = 5;

/// `GetWindowPlacement`'s show command for a window that is up and not
/// maximised, which is every window this host has.
const SW_SHOWNORMAL: u32 = 1;

/// A `WINDOWPLACEMENT` is 44 bytes on 32-bit Windows: the length, the flags, the
/// show command, two points and the normal rectangle.
const WINDOWPLACEMENT_BYTES: u32 = 44;

/// The offsets inside that structure, past the three leading `UINT`s.
const WINDOWPLACEMENT_MIN: u32 = 12;
const WINDOWPLACEMENT_MAX: u32 = 20;
const WINDOWPLACEMENT_NORMAL: u32 = 28;

/// A `WNDCLASSA` is 40 bytes and packs on four. `WNDCLASS_PROC` and
/// `WNDCLASS_NAME` are the two offsets the class registration reads, and live in
/// the parent module; the rest are named here because only this call fills them.
const WNDCLASS_BYTES: u32 = 40;
const WNDCLASS_STYLE: u32 = 0;
const WNDCLASS_CLASS_EXTRA: u32 = 8;
const WNDCLASS_WINDOW_EXTRA: u32 = 12;
const WNDCLASS_INSTANCE: u32 = 16;
const WNDCLASS_ICON: u32 = 20;
const WNDCLASS_CURSOR: u32 = 24;
const WNDCLASS_BACKGROUND: u32 = 28;
const WNDCLASS_MENU: u32 = 32;

/// Windows' boundary between an atom and a pointer: an atom is a sixteen-bit
/// number, so anything below `0x10000` names one and anything above addresses a
/// string.
const ATOM_LIMIT: u32 = 0x0001_0000;

/// The most parents a child is walked through before the walk is declared
/// circular after all: a guest that linked two windows to each other would
/// otherwise hang this call, and a fault it cannot recover from is worse than a
/// wrong answer.
const MAX_DEPTH: u32 = 64;

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "BringWindowToTop", arg_bytes: 4, shim: bring_window_to_top },
    Binding { dll: USER32, name: "CallWindowProcA", arg_bytes: 20, shim: call_window_proc },
    Binding { dll: USER32, name: "EnableWindow", arg_bytes: 8, shim: enable_window },
    Binding { dll: USER32, name: "GetActiveWindow", arg_bytes: 0, shim: get_active_window },
    Binding { dll: USER32, name: "GetCapture", arg_bytes: 0, shim: get_capture },
    Binding { dll: USER32, name: "GetClassInfoA", arg_bytes: 12, shim: get_class_info },
    Binding { dll: USER32, name: "GetClassNameA", arg_bytes: 12, shim: get_class_name },
    Binding { dll: USER32, name: "GetFocus", arg_bytes: 0, shim: get_focus },
    Binding { dll: USER32, name: "GetLastActivePopup", arg_bytes: 4, shim: get_last_active_popup },
    Binding { dll: USER32, name: "GetParent", arg_bytes: 4, shim: get_parent },
    Binding { dll: USER32, name: "GetTopWindow", arg_bytes: 4, shim: get_top_window },
    Binding { dll: USER32, name: "GetWindow", arg_bytes: 8, shim: get_window },
    Binding { dll: USER32, name: "GetWindowPlacement", arg_bytes: 8, shim: get_window_placement },
    Binding { dll: USER32, name: "GetWindowTextA", arg_bytes: 12, shim: get_window_text },
    Binding { dll: USER32, name: "GetWindowTextLengthA", arg_bytes: 4, shim: get_window_text_length },
    Binding { dll: USER32, name: "IsChild", arg_bytes: 8, shim: is_child },
    Binding { dll: USER32, name: "IsWindow", arg_bytes: 4, shim: is_window },
    Binding { dll: USER32, name: "IsWindowEnabled", arg_bytes: 4, shim: is_window_enabled },
    Binding { dll: USER32, name: "IsWindowUnicode", arg_bytes: 4, shim: is_window_unicode },
    Binding { dll: USER32, name: "IsWindowVisible", arg_bytes: 4, shim: is_window_visible },
    Binding { dll: USER32, name: "SetActiveWindow", arg_bytes: 4, shim: set_active_window },
    Binding { dll: USER32, name: "SetFocus", arg_bytes: 4, shim: set_focus },
    Binding { dll: USER32, name: "SetForegroundWindow", arg_bytes: 4, shim: set_foreground_window },
    Binding { dll: USER32, name: "ShowOwnedPopups", arg_bytes: 8, shim: show_owned_popups },
    Binding { dll: USER32, name: "UnregisterClassA", arg_bytes: 8, shim: unregister_class },
    Binding { dll: USER32, name: "WindowFromPoint", arg_bytes: 8, shim: window_from_point },
];

/// `CallWindowProcA(proc, hwnd, msg, wparam, lparam)`: runs the procedure the
/// guest passes, which is how a subclassed window reaches the one it replaced.
///
/// The routine runs in guest code, so it cannot be called from inside a shim: the
/// session runs it once this import returns, and the procedure's own value is the
/// import's answer.
fn call_window_proc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let entry = call.arg(0)?;
    if entry == 0 {
        return Ok(Return(0));
    }
    let window = call.arg(1)?;
    let message = call.arg(2)?;
    let wparam = call.arg(3)?;
    let lparam = call.arg(4)?;
    host.guest_call = Some(GuestCall {
        entry,
        args: [window, message, wparam, lparam],
        // The procedure's return value finishes the import.
        answer: None,
        resume: false,
    });
    Ok(Return(0))
}

/// `EnableWindow(hwnd, enable)`: disables or enables a window and answers whether
/// it was enabled before.
///
/// A game greys an option out by disabling it and checks the answer to decide
/// whether it has to draw it again, so the previous state is the part that
/// matters, not the new one.
fn enable_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let enable = call.arg(1)? != 0;
    Ok(Return(u32::from(host.windows.set_enabled(window, enable))))
}

/// `IsWindowEnabled(hwnd)`: whether the window accepts input.
fn is_window_enabled(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    Ok(Return(u32::from(host.windows.is_enabled(window))))
}

/// `GetActiveWindow()`: the window in the foreground of this machine's own
/// activation, falling back to the guest's window.
///
/// A guest that asked before anything was activated still gets the window it
/// created, because that is where the page's input goes: answering zero would
/// make a game conclude it is not the active application.
fn get_active_window(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let active = host.windows.active;
    Ok(Return(if active != 0 {
        active
    } else {
        host.windows.main_window()
    }))
}

/// `SetActiveWindow(hwnd)`: the most recently created window becomes active, and
/// the previous one -- which the guest usually just disabled -- is the answer.
fn set_active_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    if window != 0 && !host.windows.exists(window) {
        return Ok(Return(0));
    }
    Ok(Return(std::mem::replace(&mut host.windows.active, window)))
}

/// `GetFocus()`: the window that receives keyboard messages, falling back to the
/// guest's cross-window focus because that is where this host delivers keys.
fn get_focus(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let focus = host.windows.focus;
    Ok(Return(if focus != 0 {
        focus
    } else {
        host.windows.main_window()
    }))
}

/// `SetFocus(hwnd)`: moves the keyboard focus and answers the window that had it.
fn set_focus(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    if window != 0 && !host.windows.exists(window) {
        return Ok(Return(0));
    }
    Ok(Return(std::mem::replace(&mut host.windows.focus, window)))
}

/// `SetForegroundWindow(hwnd)`: the page cannot give a background tab the
/// foreground, so this records the guest's wish and answers the previous window.
fn set_foreground_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    if window != 0 && !host.windows.exists(window) {
        return Ok(Return(0));
    }
    Ok(Return(std::mem::replace(&mut host.windows.foreground, window)))
}

/// `GetCapture()`: the window that took the pointer, or zero when none has.
///
/// Zero is a real answer here, unlike the focus: a guest asks before it captures,
/// and a host that invented a holder would make it skip its own `SetCapture`.
fn get_capture(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.windows.capture))
}

/// `GetLastActivePopup(hwnd)`: this host has no popup stack separate from the
/// window a dialog is, so the last active popup of a window is the window.
fn get_last_active_popup(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    Ok(Return(if host.windows.exists(window) {
        window
    } else {
        0
    }))
}

/// `GetParent(hwnd)`: the window that owns this one.
///
/// A control's parent is its dialog, and a top-level window's is the desktop --
/// which is a window with a handle on Windows, and one the guest may compare
/// against `GetDesktopWindow`. Zero is reserved for the desktop's own parent.
fn get_parent(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    // A top-level window with no owner has no parent, and Windows says so with
    // NULL. Answering the desktop handle instead gives a guest a "parent" it then
    // walks -- `GetWindow(parent, GW_CHILD)`, `GetWindowLong` on what comes back --
    // and the walk ends on an address that is not there.
    Ok(Return(host.windows.parent_of(window).unwrap_or(0)))
}

/// `GetTopWindow(hwnd)`: the first window in the given window's family.
///
/// The guest's top-level window is the first thing it created, and the input the
/// page delivers goes there, so that is the answer for the desktop.
fn get_top_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    if window == 0 || window == DESKTOP_WINDOW {
        // The desktop's first child is the guest's own window, which is where
        // this host sends input.
        return Ok(Return(host.windows.main_window()));
    }
    if !host.windows.exists(window) {
        return Ok(Return(0));
    }
    // A walk down the tree -- `for (w = GetTopWindow(parent); w; w =
    // GetTopWindow(w))` -- is only finite if a window with no children answers
    // zero. Answering the window itself instead makes a launcher's start-up
    // spin on that loop forever, which is exactly how this one failed.
    Ok(Return(
        host.windows
            .children_of(window)
            .first()
            .copied()
            .unwrap_or(0),
    ))
}

/// `GetWindow(hwnd, command)`: walks the windows the guest created, in handle
/// order, which is the order it created them in.
fn get_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let command = call.arg(1)?;
    let mut order: Vec<u32> = host.windows.windows.keys().copied().collect();
    order.sort_unstable();
    let position = order.iter().position(|candidate| *candidate == window);
    let answer = match command {
        GW_HWNDFIRST => order.first().copied(),
        GW_HWNDLAST => order.last().copied(),
        GW_HWNDNEXT => position.and_then(|at| order.get(at + 1)).copied(),
        GW_HWNDPREV => position
            .and_then(|at| at.checked_sub(1).and_then(|before| order.get(before)))
            .copied(),
        GW_OWNER => match host.windows.owner_of(window) {
            0 => None,
            owner => Some(owner),
        },
        // The desktop's children are the top-level windows a guest created; the
        // first of them is the one it calls its main window.
        GW_CHILD if window == DESKTOP_WINDOW => Some(host.windows.main_window()),
        GW_CHILD => host.windows.children_of(window).first().copied(),
        _ => None,
    };
    Ok(Return(answer.unwrap_or(0)))
}

/// `GetWindowPlacement(hwnd, placement)`: where the window sits and how it is
/// shown.
///
/// A game reads this to find the screen it should draw on when it does not trust
/// `GetClientRect`, and the normal rectangle is the one value here that is real;
/// there is no minimised or maximised window on this machine, so the show command
/// is always `SW_SHOWNORMAL`.
fn get_window_placement(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let destination = call.arg(1)?;
    let Some(rect) = host.windows.window_rect(window) else {
        return Ok(Return(0));
    };
    if destination == 0 {
        return Ok(Return(0));
    }
    let mut out = vec![0_u8; WINDOWPLACEMENT_BYTES as usize];
    out[0..4].copy_from_slice(&WINDOWPLACEMENT_BYTES.to_le_bytes());
    // flags stay zero: no window is minimised or maximised.
    out[8..12].copy_from_slice(&SW_SHOWNORMAL.to_le_bytes());
    // Both extreme positions are -1,-1 when they are not set, which is what
    // Windows reports for a window that was never minimised.
    let unset = [
        WINDOWPLACEMENT_MIN,
        WINDOWPLACEMENT_MIN + 4,
        WINDOWPLACEMENT_MAX,
        WINDOWPLACEMENT_MAX + 4,
    ];
    for at in unset {
        store(&mut out, at, u32::MAX);
    }
    for (offset, value) in rect.iter().enumerate() {
        store(&mut out, WINDOWPLACEMENT_NORMAL + offset as u32 * 4, *value as u32);
    }
    call.write(destination, &out)?;
    Ok(Return(1))
}

/// `GetClassNameA(hwnd, buffer, max)`: the name the window's class was registered
/// under, which is what a guest compares to tell a control from its dialog.
fn get_class_name(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let buffer = call.arg(1)?;
    let max = call.arg(2)?;
    let Some(class) = host.windows.class_of(window).map(str::to_owned) else {
        return Ok(Return(0));
    };
    Ok(Return(copy_text(call, buffer, max, &class)?))
}

/// `GetClassInfoA(instance, name, class)`: fills a `WNDCLASSA` for a registered
/// class, which a guest uses to subclass a control it did not write.
///
/// Only the procedure and the name are this host's to report: the extra bytes are
/// kept by the guest itself, there are no icons, cursors, backgrounds or menu
/// resources behind the class, so those fields are zero -- and a guest that reads
/// them back gets the zero it handed in, which is what it handed in.
fn get_class_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let instance = call.arg(0)?;
    let name_argument = call.arg(1)?;
    let destination = call.arg(2)?;
    let Some(name) = class_name(host, name_argument, call) else {
        return Ok(Return(0));
    };
    let Some(procedure) = host.windows.class_proc(&name) else {
        return Ok(Return(0));
    };
    if destination == 0 {
        return Ok(Return(0));
    }
    let mut out = vec![0_u8; WNDCLASS_BYTES as usize];
    store(&mut out, WNDCLASS_STYLE, 0);
    store(&mut out, WNDCLASS_PROC, procedure);
    store(&mut out, WNDCLASS_CLASS_EXTRA, 0);
    store(&mut out, WNDCLASS_WINDOW_EXTRA, 0);
    store(&mut out, WNDCLASS_INSTANCE, instance);
    store(&mut out, WNDCLASS_ICON, 0);
    store(&mut out, WNDCLASS_CURSOR, 0);
    store(&mut out, WNDCLASS_BACKGROUND, 0);
    store(&mut out, WNDCLASS_MENU, 0);
    // The class name is the string the guest passed, so the pointer it gets back
    // is the one it already owns; an atom names no string to point at.
    let class_pointer = if name_argument >= ATOM_LIMIT {
        name_argument
    } else {
        0
    };
    store(&mut out, WNDCLASS_NAME, class_pointer);
    call.write(destination, &out)?;
    Ok(Return(1))
}

/// `GetWindowTextA(hwnd, buffer, max)`: the caption of the guest's window, or the
/// label of a control.
fn get_window_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let buffer = call.arg(1)?;
    let max = call.arg(2)?;
    let Some(text) = host.windows.text_of(window).map(str::to_owned) else {
        return Ok(Return(0));
    };
    Ok(Return(copy_text(call, buffer, max, &text)?))
}

/// `GetWindowTextLengthA(hwnd)`: how long that text is.
///
/// The length is the bytes the `A` form copies, not the characters, because that
/// is the buffer size a guest allocates before asking. Every caption a game sets
/// is ASCII, so the two are the same here.
fn get_window_text_length(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    Ok(Return(
        host.windows
            .text_of(window)
            .map_or(0, |text| text.len() as u32),
    ))
}

/// `IsChild(parent, child)`: whether the second window is somewhere below the
/// first, which a dialog asks before trusting a handle it was handed.
fn is_child(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parent = call.arg(0)?;
    let mut window = call.arg(1)?;
    for _ in 0..MAX_DEPTH {
        let Some(owner) = host.windows.parent_of(window) else {
            break;
        };
        if owner == parent {
            return Ok(Return(1));
        }
        window = owner;
    }
    Ok(Return(0))
}

/// `IsWindow(hwnd)`: whether the handle names a window this host knows.
///
/// This is the check a guest makes before using a handle it stored earlier, so it
/// has to say yes for a window that exists but is disabled or hidden.
fn is_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    Ok(Return(u32::from(host.windows.exists(window))))
}

/// `IsWindowVisible(hwnd)`.
///
/// The desktop is always visible -- it is the screen the page presents -- and a
/// window is visible once the guest has shown it.
fn is_window_visible(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    if window == DESKTOP_WINDOW {
        return Ok(Return(1));
    }
    Ok(Return(u32::from(host.windows.is_visible(window))))
}

/// `IsWindowUnicode(hwnd)`: this host's windows are ANSI.
///
/// Every entry point a guest reaches for text here is the `A` form, so answering
/// yes would send a guest that branches on this down a `W` path whose shims are
/// the same functions reading different bytes.
fn is_window_unicode(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `BringWindowToTop(hwnd)`: this host has one visible surface, so the ordering
/// of windows is a fact the page cannot show -- it is recorded as a success so a
/// guest does not conclude the window is gone.
fn bring_window_to_top(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    Ok(Return(u32::from(host.windows.exists(window))))
}

/// `ShowOwnedPopups(hwnd, show)`: the same booking, for a window's popups.
fn show_owned_popups(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    Ok(Return(u32::from(host.windows.exists(window))))
}

/// `UnregisterClassA(name, instance)`: forgets a class, so the next
/// `CreateWindowExA` of that name fails the way it does on Windows.
fn unregister_class(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name_argument = call.arg(0)?;
    let Some(name) = class_name(host, name_argument, call) else {
        return Ok(Return(0));
    };
    Ok(Return(u32::from(host.windows.unregister_class(&name))))
}

/// `WindowFromPoint(point)`: the window a screen position lands in.
///
/// The point is passed by value, as two 32-bit integers, not by pointer. The
/// smallest window containing the point is the answer -- a control before the
/// dialog it sits in -- which is the order the user's click actually arrives in.
fn window_from_point(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let x = call.arg(0)? as i32;
    let y = call.arg(1)? as i32;
    Ok(Return(host.windows.window_at(x, y).unwrap_or(0)))
}

/// The class name an argument names: a pointer to a string, or one of the atom
/// numbers `RegisterClassA` answered -- which this host files as the index of the
/// class in its own table rather than as a real atom, so the two have to be told
/// apart the way Windows does, by the value.
fn class_name(host: &Host, argument: u32, call: &Call<'_>) -> Option<String> {
    if argument >= ATOM_LIMIT {
        return call.memory.read_cstr(argument).ok();
    }
    let index = (argument as usize).checked_sub(1)?;
    host.windows
        .classes
        .get(index)
        .map(|(class, _)| class.clone())
}

/// Writes one 32-bit field into a structure being filled.
fn store(out: &mut [u8], offset: u32, value: u32) {
    out[offset as usize..offset as usize + 4].copy_from_slice(&value.to_le_bytes());
}

/// Copies text into a guest buffer the way the `A` entry points do: NUL
/// terminated, at most `max - 1` bytes, answering the bytes copied. A null buffer
/// or a non-positive `max` copies nothing, which is what Windows does with them.
fn copy_text(call: &mut Call<'_>, buffer: u32, max: u32, text: &str) -> Result<u32> {
    let max = max as i32;
    if buffer == 0 || max <= 0 {
        return Ok(0);
    }
    let bytes = text.as_bytes();
    let mut take = bytes.len().min(max as usize - 1);
    // Truncation may not split a character: a caption with a character this host
    // received as UTF-8 would otherwise end in half of one.
    while take > 0 && !text.is_char_boundary(take) {
        take -= 1;
    }
    let mut out = Vec::with_capacity(take + 1);
    out.extend_from_slice(&bytes[..take]);
    out.push(0);
    call.write(buffer, &out)?;
    Ok(take as u32)
}
