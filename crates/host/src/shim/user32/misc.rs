//! The `USER32.dll` calls that are neither drawing nor the window queries: the
//! per-window property table, the system colours, `SetTimer`, the hook chain and
//! the message-name registry.
//!
//! Two of these are load-bearing for a game of this era. **Timers** are how a
//! menu opens on a hover or a cursor blinks: `SetTimer` registers one here and
//! the pump in [`super`] posts the `WM_TIMER` when it comes due, so the guest's
//! own `PeekMessageA` loop sees it. **Properties** are the per-window table a
//! program that has no room left in its own structures uses instead of a global:
//! `SetPropA` and `GetPropA` have to agree, or the pointer a window stashes at
//! creation comes back as null in its window procedure.
//!
//! The colours are the classic system palette rather than a theme, because the
//! guest compares against them to decide whether text is readable on the ground
//! it just painted.

use super::Hook;
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const USER32: &str = "USER32.dll";

/// `COLOR_*` indices, with the colour Windows gives each in the classic scheme.
///
/// The list is in `COLORREF` order -- `0x00BBGGRR` -- which is the packing a
/// guest passes back to `SetTextColor` unchanged. A value that is not here
/// answers black, which is also what Windows answers for an unset entry, so a
/// guest that indexes past the end gets a colour rather than a fault.
fn system_colour(index: u32) -> u32 {
    match index {
        0 => 0x00c0_c0c0,  // SCROLLBAR: the button grey
        1 => 0x0080_8080,  // BACKGROUND: the classic teal desktop
        2 => 0x0080_0000,  // ACTIVECAPTION: navy
        3 => 0x00c0_c0c0,  // INACTIVECAPTION
        4 => 0x00c0_c0c0,  // MENU: the ground a menu bar is drawn on
        5 => 0x00ff_ffff,  // WINDOW: white, the paper of an edit control
        6 => 0x0000_0000,  // WINDOWFRAME
        7 => 0x0000_0000,  // MENUTEXT
        8 => 0x0000_0000,  // WINDOWTEXT
        9 => 0x00ff_ffff,  // CAPTIONTEXT
        10 => 0x00c0_c0c0, // ACTIVEBORDER
        11 => 0x00c0_c0c0, // INACTIVEBORDER
        12 => 0x00c0_c0c0, // APPWORKSPACE
        13 => 0x0080_0000, // HIGHLIGHT: the selection navy
        14 => 0x00ff_ffff, // HIGHLIGHTTEXT
        15 => 0x00c0_c0c0, // BTNFACE: the dialog grey
        16 => 0x0080_8080, // BTNSHADOW
        17 => 0x0080_8080, // GRAYTEXT: text a disabled control draws
        18 => 0x0000_0000, // BTNTEXT
        19 => 0x00c0_c0c0, // INACTIVECAPTIONTEXT
        20 => 0x00ff_ffff, // BTNHIGHLIGHT
        21 => 0x0000_0000, // 3DDKSHADOW
        22 => 0x00c0_c0c0, // 3DLIGHT
        23 => 0x0000_0000, // INFOTEXT
        24 => 0x00e1_ffff, // INFOBK: the pale yellow of a tooltip
        _ => 0x0000_0000,
    }
}

/// The stock-object space GDI's handles live in, which is where
/// `GetSysColorBrush` answers from: `0x0002_0000` is `GetStockObject`'s first
/// object, and every index above it steps by four.
const STOCK_OBJECT_BASE: u32 = 0x0002_0000;

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "CallNextHookEx", arg_bytes: 16, shim: call_next_hook_ex },
    Binding { dll: USER32, name: "GetMessagePos", arg_bytes: 0, shim: get_message_pos },
    Binding { dll: USER32, name: "GetMessageTime", arg_bytes: 0, shim: get_message_time },
    Binding { dll: USER32, name: "GetPropA", arg_bytes: 8, shim: get_prop },
    Binding { dll: USER32, name: "GetSysColor", arg_bytes: 4, shim: get_sys_color },
    Binding { dll: USER32, name: "GetSysColorBrush", arg_bytes: 4, shim: get_sys_color_brush },
    Binding { dll: USER32, name: "KillTimer", arg_bytes: 8, shim: kill_timer },
    Binding { dll: USER32, name: "RegisterWindowMessageA", arg_bytes: 4, shim: register_window_message },
    Binding { dll: USER32, name: "RemovePropA", arg_bytes: 8, shim: remove_prop },
    Binding { dll: USER32, name: "SendNotifyMessageA", arg_bytes: 16, shim: send_notify_message },
    Binding { dll: USER32, name: "SetPropA", arg_bytes: 12, shim: set_prop },
    Binding { dll: USER32, name: "SetTimer", arg_bytes: 16, shim: set_timer },
    Binding { dll: USER32, name: "SetWindowsHookExA", arg_bytes: 16, shim: set_windows_hook_ex },
    Binding { dll: USER32, name: "UnhookWindowsHookEx", arg_bytes: 4, shim: unhook_windows_hook_ex },
    Binding { dll: USER32, name: "WaitMessage", arg_bytes: 0, shim: wait_message },
];

/// `SetPropA(hwnd, name, data)`: files a value under a name on one window.
///
/// A game of this era has no room for a member variable per window and stores
/// its own pointer this way, then reads it back in the window procedure -- which
/// runs on the same host, so the table has to answer exactly what was stored
/// even across a `DispatchMessageA`.
fn set_prop(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let name = call.cstr(1)?;
    let data = call.arg(2)?;
    host.windows.props.insert((window, name), data);
    Ok(Return(1))
}

fn get_prop(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let name = call.cstr(1)?;
    Ok(Return(
        host.windows
            .props
            .get(&(window, name))
            .copied()
            .unwrap_or(0),
    ))
}

/// `RemovePropA(hwnd, name)`: the value it removed, or zero when there was none.
fn remove_prop(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let name = call.cstr(1)?;
    Ok(Return(
        host.windows.props.remove(&(window, name)).unwrap_or(0),
    ))
}

fn get_sys_color(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(system_colour(call.arg(0)?)))
}

/// `GetSysColorBrush(index)`: a brush handle for a system colour, which a guest
/// selects into a context and fills with.
///
/// The handle comes from the same space `GetStockObject` answers from, and
/// `SelectObject` and `DeleteObject` accept a stock handle here without asking
/// which kind it is -- which is what makes this a handle rather than a colour.
fn get_sys_color_brush(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(STOCK_OBJECT_BASE + call.arg(0)?.min(63) * 4))
}

/// `SetTimer(hwnd, id, elapse, proc)`: registers a timer and answers its id.
///
/// This is the one call here that the guest can see happen: the pump asks the
/// same table for anything due, so an animation driven by a `WM_TIMER` advances
/// on the guest's own `PeekMessageA` loop rather than never.
///
/// A null window with a null procedure is the one combination Windows refuses,
/// because nothing would receive the message and nothing would be called.
fn set_timer(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let id = call.arg(1)?;
    let interval = call.arg(2)?;
    let proc_address = call.arg(3)?;
    if window == 0 && proc_address == 0 {
        return Ok(Return(0));
    }
    let now = host.now();
    let id = host
        .windows
        .add_timer(window, id, interval, proc_address, now);
    host.wake_message_waiters();
    Ok(Return(id))
}

/// `KillTimer(hwnd, id)`: whether a timer of that id was there to remove.
fn kill_timer(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let id = call.arg(1)?;
    Ok(Return(u32::from(host.windows.kill_timer(window, id))))
}

/// `RegisterWindowMessageA(name)`: one number per name, for the life of the
/// session.
///
/// Two windows message each other by a name both agree on, and the number has to
/// match on both sides -- so the registry hands out the first number it gave a
/// name and keeps it, rather than a fresh one per call.
fn register_window_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.cstr(0)?;
    Ok(Return(host.windows.register_message(&name)))
}

/// `GetMessagePos`: where the pointer was when the message the pump last handed
/// out was produced, packed as `MAKELONG(x, y)`.
///
/// The guest uses it instead of `GetCursorPos` for a click, because the pointer
/// has moved on by the time the message is processed and the position in the
/// message is the one the click happened at.
fn get_message_pos(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let (x, y) = host.windows.message_pos;
    Ok(Return((x as u32 & 0xffff) | ((y as u32 & 0xffff) << 16)))
}

fn get_message_time(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.windows.message_time))
}

/// `SendNotifyMessageA(hwnd, msg, wparam, lparam)`.
///
/// Windows delivers this without waiting for it to be handled, so the message is
/// queued rather than dispatched into the window procedure here: the guest's own
/// pump then sees it on its next iteration, which is what "post it and move on"
/// looks like from inside the guest.
fn send_notify_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let message = call.arg(1)?;
    let wparam = call.arg(2)?;
    let lparam = call.arg(3)?;
    host.input.post(window, message, wparam, lparam);
    host.wake_message_waiters();
    Ok(Return(1))
}

/// `SetWindowsHookExA(idHook, proc, module, thread)`: installs a hook and
/// answers its handle.
///
/// There is no other process on this machine to filter, so a hook is remembered
/// and never called from outside: what matters is that the handle is a number a
/// guest can pass to `UnhookWindowsHookEx` and get a success for.
fn set_windows_hook_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let kind = call.arg(0)?;
    let proc_address = call.arg(1)?;
    let module = call.arg(2)?;
    let thread = call.arg(3)?;
    let handle = host.windows.add_hook(Hook {
        kind: kind as i32,
        thread,
        proc_address,
        module,
    });
    Ok(Return(handle))
}

fn unhook_windows_hook_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from(
        host.windows.remove_hook(call.arg(0)?),
    )))
}

/// `CallNextHookEx(hhk, code, wparam, lparam)`: zero, the end of the chain.
///
/// A hook procedure passes a message on when it does not handle it, and the last
/// one in a chain answers zero for the message hooks -- which is what this host
/// is, since it chains no other process's hooks.
fn call_next_hook_ex(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `WaitMessage()`: answers at once, successfully.
///
/// Windows blocks until the queue has something in it. This host's pump cannot
/// park: the page cannot interrupt a call, and `GetMessageA` already answers a
/// placeholder instead of blocking so the guest's own loop keeps drawing frames.
/// Answering success here is the same contract -- a guest that treats a zero as
/// "the wait failed" would otherwise leave its message loop for good.
fn wait_message(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}
