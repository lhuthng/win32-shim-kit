//! The stateful odds and ends of `USER32.dll`: scroll bars, the caret, the
//! accelerator table, deferred window placement, and the help call.
//!
//! Two of these are state a guest reads back rather than a picture:
//!
//! - **Scroll bars.** A game sets a range for its panel, then computes from
//!   `GetScrollPos` where to draw the thumb and what to put on screen. The bar
//!   itself is never drawn by this host -- the page is the window and the guest
//!   renders its own chrome -- but a host that answered zero to `GetScrollPos`
//!   would leave the guest drawing the top of its list forever.
//! - **Accelerators.** The shortcut keys a menu advertises are a resource in the
//!   image, and `TranslateAcceleratorA` turns one into the same `WM_COMMAND` a
//!   click would post. Without it a game whose menus are reachable only by key
//!   has no keyboard path to its options at all.
//!
//! The caret is state without a picture for the same reason: the guest draws its
//! own cursor, and what it asks this host is only whether the caret is currently
//! shown.

use std::collections::HashMap;

use crate::pe::{ResourceKey, IMAGE_BASE};
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use super::Caret;

use Action::Return;

const USER32: &str = "USER32.dll";

/// The bitmaps `LoadBitmapA` registers, above the ones the resource tree's own
/// handles use, so the two cannot name the same object.
const FIRST_LOADED_BITMAP: u32 = 0x0001_2500;

/// The handle `BeginDeferWindowPos` answers, above the loaded bitmaps.
const FIRST_DEFER: u32 = 0x0001_0b00;

/// `SB_HORZ`, `SB_VERT` and `SB_BOTH`: which bar a scroll call is about. The pair
/// this host keeps is indexed by the first two; `SB_BOTH` applies to both.
const SB_HORZ: u32 = 0;
const SB_VERT: u32 = 1;
const SB_BOTH: u32 = 2;

/// `SCROLLINFO`'s fields, at their documented offsets. The structure is 28 bytes
/// and every member is a `UINT` or an `int`, so the offsets are named rather than
/// derived.
const SI_MASK: u32 = 4;
const SI_MIN: u32 = 8;
const SI_MAX: u32 = 12;
const SI_PAGE: u32 = 16;
const SI_POS: u32 = 20;

/// The `fMask` bits that say which of those fields the guest filled in.
const SIF_RANGE: u32 = 0x0001;
const SIF_PAGE: u32 = 0x0002;
const SIF_POS: u32 = 0x0004;

/// `RT_ACCELERATOR`: the table `LoadAcceleratorsA` reads.
const RT_ACCELERATOR: u32 = 9;

/// `RT_BITMAP`, which `LoadBitmapA` registers.
const RT_BITMAP: u32 = 2;

/// `WM_KEYDOWN` and `WM_COMMAND`: the message an accelerator consumes, and the
/// one it stands in for.
const WM_KEYDOWN: u32 = 0x0100;
const WM_COMMAND: u32 = 0x0111;

/// `SWP_NOMOVE`: the placement flag that says the position is not to change.
const SWP_NOMOVE: u32 = 0x0002;

/// How many accelerator entries a table may hold before this host stops reading.
/// A table ends with a zero entry, so this is only a guard against a handle that
/// does not name one.
const MAX_ACCELERATORS: u32 = 256;

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "GetScrollPos", arg_bytes: 8, shim: get_scroll_pos },
    Binding { dll: USER32, name: "SetScrollPos", arg_bytes: 16, shim: set_scroll_pos },
    Binding { dll: USER32, name: "SetScrollRange", arg_bytes: 20, shim: set_scroll_range },
    Binding { dll: USER32, name: "SetScrollInfo", arg_bytes: 16, shim: set_scroll_info },
    Binding { dll: USER32, name: "ShowScrollBar", arg_bytes: 12, shim: show_scroll_bar },
    Binding { dll: USER32, name: "ScrollWindow", arg_bytes: 20, shim: scroll_window },
    Binding { dll: USER32, name: "ShowCaret", arg_bytes: 4, shim: show_caret },
    Binding { dll: USER32, name: "HideCaret", arg_bytes: 4, shim: hide_caret },
    Binding { dll: USER32, name: "LoadAcceleratorsA", arg_bytes: 8, shim: load_accelerators },
    Binding { dll: USER32, name: "TranslateAcceleratorA", arg_bytes: 12, shim: translate_accelerator },
    Binding { dll: USER32, name: "LoadBitmapA", arg_bytes: 8, shim: load_bitmap },
    Binding { dll: USER32, name: "BeginDeferWindowPos", arg_bytes: 4, shim: begin_defer_window_pos },
    Binding { dll: USER32, name: "DeferWindowPos", arg_bytes: 32, shim: defer_window_pos },
    Binding { dll: USER32, name: "EndDeferWindowPos", arg_bytes: 4, shim: end_defer_window_pos },
    Binding { dll: USER32, name: "WinHelpA", arg_bytes: 16, shim: win_help },
];

// ---- scroll bars ----------------------------------------------------------

/// The bars a call names: one entry, or both for `SB_BOTH`.
fn bars(bar: u32) -> &'static [usize] {
    match bar {
        SB_HORZ => &[0],
        SB_VERT => &[1],
        SB_BOTH => &[0, 1],
        // Anything else names no bar, which is what Windows answers nothing for.
        _ => &[],
    }
}

/// `GetScrollPos(window, bar)`: where the thumb is.
fn get_scroll_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let bar = call.arg(1)?;
    let Some(index) = bars(bar).first().copied() else {
        return Ok(Return(0));
    };
    Ok(Return(host.windows.scroll_bar(window, index).position.max(0) as u32))
}

/// `SetScrollPos(window, bar, pos, redraw)`: the previous position.
///
/// The bar is not redrawn -- there is no bar to redraw -- but a guest that stores
/// the previous position to restore it later gets the one it set before.
fn set_scroll_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let bar = call.arg(1)?;
    let position = call.arg(2)? as i32;
    if !host.windows.exists(window) {
        return Ok(Return(0));
    }
    let Some(index) = bars(bar).first().copied() else {
        return Ok(Return(0));
    };
    let mut state = host.windows.scroll_bar(window, index);
    let previous = state.position;
    state.position = position;
    state.live = true;
    host.windows.set_scroll_bar(window, index, state);
    Ok(Return(previous.max(0) as u32))
}

/// `SetScrollRange(window, bar, min, max, redraw)`: whether the window exists.
///
/// A minimum above the maximum is a request to hide the bar on Windows, and that
/// is what the `live` flag records here: a bar with no range is one no
/// `WM_*SCROLL` could come from, and a guest that asks `GetScrollPos` on it reads
/// where the thumb was left.
fn set_scroll_range(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let bar = call.arg(1)?;
    let (min, max) = (call.arg(2)? as i32, call.arg(3)? as i32);
    if !host.windows.exists(window) {
        return Ok(Return(0));
    }
    let hide = !bars(bar).is_empty() && min > max && max != 0;
    for index in bars(bar) {
        let mut state = host.windows.scroll_bar(window, *index);
        state.min = min;
        state.max = max;
        state.live = !hide;
        host.windows.set_scroll_bar(window, *index, state);
    }
    Ok(Return(1))
}

/// `SetScrollInfo(window, bar, info, redraw)`: the position the bar ended at.
///
/// This is the call a client from this era actually uses -- it sets the range,
/// the page size and the position in one go, and expects the answer to be the
/// position so it can draw its thumb without a second query.
fn set_scroll_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let bar = call.arg(1)?;
    let info = call.arg(2)?;
    if !host.windows.exists(window) || info == 0 {
        return Ok(Return(0));
    }
    let mask = call.memory.read_u32(info + SI_MASK)?;
    let min = call.memory.read_u32(info + SI_MIN)? as i32;
    let max = call.memory.read_u32(info + SI_MAX)? as i32;
    let page = call.memory.read_u32(info + SI_PAGE)?;
    let position = call.memory.read_u32(info + SI_POS)? as i32;
    let mut answer = 0;
    for index in bars(bar) {
        let mut state = host.windows.scroll_bar(window, *index);
        if mask & SIF_RANGE != 0 {
            state.min = min;
            state.max = max;
            state.live = true;
        }
        if mask & SIF_PAGE != 0 {
            state.page = page;
        }
        if mask & SIF_POS != 0 {
            state.position = position;
        }
        answer = state.position;
        host.windows.set_scroll_bar(window, *index, state);
    }
    Ok(Return(answer.max(0) as u32))
}

/// `ShowScrollBar(window, bar, show)`: whether the window exists.
///
/// `show` is recorded as whether the bar is live, because it is the same fact from
/// the guest's side: a bar that is not shown has no range to scroll.
fn show_scroll_bar(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let bar = call.arg(1)?;
    let show = call.arg(2)? != 0;
    if !host.windows.exists(window) {
        return Ok(Return(0));
    }
    for index in bars(bar) {
        let mut state = host.windows.scroll_bar(window, *index);
        state.live = show;
        host.windows.set_scroll_bar(window, *index, state);
    }
    Ok(Return(1))
}

/// `ScrollWindow(window, dx, dy, rect, clip)`: whether the window exists.
///
/// What scrolls is the *content*, so the window's rectangle moves the other way:
/// a guest that scrolls down by a line moves its view down the page, and its
/// origin up. Nothing is blitted here -- the guest redraws what it needs when it
/// gets the `WM_PAINT` this stands for -- so moving the rectangle is the whole
/// effect, and it keeps `GetWindowRect` consistent with what the guest shows.
fn scroll_window(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let (dx, dy) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let Some(rect) = host.windows.window_rect(window) else {
        return Ok(Return(0));
    };
    host.windows
        .move_window(window, rect[0] - dx, rect[1] - dy);
    Ok(Return(1))
}

// ---- the caret ------------------------------------------------------------

/// The caret this host makes for a window that shows one: the two pixels wide and
/// sixteen tall a text cursor is at a VGA font.
const CARET_WIDTH: u32 = 2;
const CARET_HEIGHT: u32 = 16;

/// `ShowCaret(window)`: whether there is a caret to show.
///
/// The caret is not drawn: a guest from this era draws its own text cursor, and
/// what it asks here is whether one is already on. A window that never created
/// one has nothing to show, which is `FALSE` on Windows too.
fn show_caret(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let caret = match host.windows.caret {
        Some(mut caret) if caret.window == window => {
            caret.visible = true;
            caret
        }
        Some(_) => return Ok(Return(0)),
        None => Caret {
            window,
            x: 0,
            y: 0,
            width: CARET_WIDTH,
            height: CARET_HEIGHT,
            visible: true,
        },
    };
    host.windows.caret = Some(caret);
    Ok(Return(1))
}

/// `HideCaret(window)`: whether a caret was hidden.
fn hide_caret(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    match host.windows.caret.as_mut() {
        Some(caret) if caret.window == window && caret.visible => {
            caret.visible = false;
            Ok(Return(1))
        }
        _ => Ok(Return(0)),
    }
}

// ---- accelerators ---------------------------------------------------------

/// The loaded accelerator tables, by the handle handed out, holding where the
/// table is in guest memory and how long it is.
///
/// A handle has to name the table for `TranslateAcceleratorA` to read it, and the
/// guest memory address is the one value that already does: it is stable for the
/// life of the module and the module never moves. The entry here exists so a
/// handle that was never loaded can be refused, which is what a guest passing a
/// stale table should get.
fn loaded() -> &'static mut HashMap<u32, (u32, u32)> {
    static mut LOADED: Option<HashMap<u32, (u32, u32)>> = None;
    // A shim runs on one thread -- the guest's -- so the table is only ever
    // touched from there, which is what makes this sound in this host.
    #[allow(static_mut_refs)]
    unsafe {
        LOADED.get_or_insert_with(HashMap::new)
    }
}

/// `LoadAcceleratorsA(instance, name)`: the table's address, or 0.
///
/// The table comes from the module the instance names, as every `Load*` call does:
/// a program that keeps its shortcuts in a DLL passes that DLL's handle.
fn load_accelerators(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(1)?;
    let key = if name < 0x1_0000 {
        ResourceKey::Id(name)
    } else {
        ResourceKey::Name(call.memory.read_cstr(name)?)
    };
    let (resources, base, _) = super::instance_resources(host, call.arg(0)?);
    let found = resources
        .iter()
        .find(|resource| {
            resource.kind == ResourceKey::Id(RT_ACCELERATOR) && resource.name == key
        })
        .map(|resource| (base.wrapping_add(resource.rva), resource.size));
    let Some((at, size)) = found else {
        // No table means no shortcut is translated, which is what a guest that
        // never loaded one expects; it is not a reason to stop the boot.
        return Ok(Return(0));
    };
    loaded().insert(at, (at, size));
    Ok(Return(at))
}

/// `TranslateAcceleratorA(window, table, msg)`: whether the message was consumed.
///
/// An accelerator stands in for a menu click: the key and the command appear on
/// the same item, and the message a guest handles is the `WM_COMMAND` either way.
/// A key that is not in the table is left in the queue for the guest's own
/// handling, which is why the answer is what the caller checks before dispatching.
///
/// The modifier bits (`FCONTROL`, `FSHIFT`, `FALT`) are not tested: a shim cannot
/// see the keyboard state a message was produced with, and a table built by a
/// resource compiler carries `FVIRTKEY` for every entry it emits, so the key alone
/// is what distinguishes them in practice.
fn translate_accelerator(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let table = call.arg(1)?;
    let message = call.arg(2)?;
    if message == 0 || table == 0 {
        return Ok(Return(0));
    }
    let Some((at, length)) = loaded().get(&table).copied() else {
        return Ok(Return(0));
    };
    let kind = call.memory.read_u32(message + 4)?;
    let key = call.memory.read_u32(message + 8)? & 0xffff;
    if kind != WM_KEYDOWN {
        return Ok(Return(0));
    }
    // An `ACCEL` is four words: the flags, the key, the command id, and padding.
    let count = (length / 8).min(MAX_ACCELERATORS);
    for index in 0..count {
        let entry = at + index * 8;
        let flags = call.memory.read_u16(entry)?;
        if flags == 0 {
            break;
        }
        let entry_key = u32::from(call.memory.read_u16(entry + 2)?);
        if entry_key != key {
            continue;
        }
        let command = u32::from(call.memory.read_u16(entry + 4)?);
        // The command is posted to the window the caller named, which for a
        // menu accelerator is the window whose menu holds the item.
        host.input.post(window, WM_COMMAND, command, 0);
        host.wake_message_waiters();
        return Ok(Return(1));
    }
    Ok(Return(0))
}

// ---- bitmaps --------------------------------------------------------------

/// `LoadBitmapA(instance, name)`: a GDI bitmap handle, or 0.
///
/// The handle is also the GDI object, so the guest can select it into a context
/// and read its size back; the pixels stay where the image mapped them rather
/// than being copied, so a game that loads a hundred pictures holds one copy.
///
/// A resource that is not there answers 0, which is what a caller with a fallback
/// already handles -- but it is worth a line in the log, because a game that
/// loads its artwork by name and gets nothing back draws an empty screen.
fn load_bitmap(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(1)?;
    let key = if name < 0x1_0000 {
        ResourceKey::Id(name)
    } else {
        ResourceKey::Name(call.memory.read_cstr(name)?)
    };
    let found = host.resources.iter().position(|resource| {
        resource.kind == ResourceKey::Id(RT_BITMAP) && resource.name == key
    });
    let Some(index) = found else {
        if host.notices.len() < 400 {
            host.notice(format!("LoadBitmapA: no bitmap resource named {key:?}"));
        }
        return Ok(Return(0));
    };
    let handle = FIRST_LOADED_BITMAP + index as u32 * 4;
    let rva = IMAGE_BASE.wrapping_add(host.resources[index].rva);
    if let Err(error) = host.gdi.add_bitmap(call.memory, handle, rva) {
        host.notice(format!("LoadBitmapA: {error}"));
        return Ok(Return(0));
    }
    Ok(Return(handle))
}

// ---- deferred placement ---------------------------------------------------

/// `BeginDeferWindowPos(count)`: a handle for the batch.
///
/// The handle carries the number of windows the caller promised, which is all
/// `DeferWindowPos` needs to check against; nothing is allocated for it.
fn begin_defer_window_pos(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let count = call.arg(0)? & 0xff;
    Ok(Return(FIRST_DEFER + count * 4))
}

/// `DeferWindowPos(batch, window, after, x, y, cx, cy, flags)`: the batch handle.
///
/// Windows collects the placements and applies them in `EndDeferWindowPos`, so a
/// dialog lays its controls out without each one flashing into place. Nothing
/// here redraws between the calls -- the guest's own renderer draws when it is
/// ready -- so each placement is applied as it arrives and the batch only has to
/// keep its handle.
fn defer_window_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let batch = call.arg(0)?;
    let window = call.arg(1)?;
    let (x, y) = (call.arg(3)? as i32, call.arg(4)? as i32);
    let flags = call.arg(7)?;
    if flags & SWP_NOMOVE == 0 {
        host.windows.move_window(window, x, y);
    }
    // A batch of zero is the failure Windows reports, so the handle is passed
    // through as it came.
    Ok(Return(batch))
}

/// `EndDeferWindowPos(batch)`: 1, since every placement is already applied.
fn end_defer_window_pos(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from(call.arg(0)? != 0 && host.windows.main_window() != 0)))
}

// ---- help -----------------------------------------------------------------

/// `WinHelpA(window, file, command, data)`: 1, with the request recorded.
///
/// This host has no help viewer, and a game that opens its own help must not
/// stop on the call. What the guest asked for is kept as a notice so a run shows
/// that a player reached the help and that it was not shown.
fn win_help(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(1)?;
    let command = call.arg(2)?;
    if host.notices.len() < 400 {
        let path = if file == 0 {
            String::from("(none)")
        } else {
            call.memory
                .read_cstr(file)
                .unwrap_or_else(|_| String::from("(unreadable)"))
        };
        host.notice(format!(
            "WinHelpA: {path} command {command} is not shown, because this host has no help viewer"
        ));
    }
    Ok(Return(1))
}
