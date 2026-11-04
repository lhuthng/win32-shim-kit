//! The `USER32.dll` rectangle and point arithmetic, and the DDE parameter
//! packers that ship in the same DLL.
//!
//! These are the functions a menu, a dialog and a scrolling panel are laid out
//! with, and they are pure arithmetic on `RECT` and `POINT`: four `LONG`s and two,
//! in naive guest memory. Nothing here draws, so nothing here needs the display —
//! which is the point, because the guest computes every position its later `Blit`
//! and `FillRect` will use, and a single wrong `InflateRect` puts a whole menu one
//! pixel off.
//!
//! A null pointer is answered with zero rather than read: Win32 fails a call that
//! was given nowhere to write, and a guest that passes one is the guest's problem,
//! not a fault in this machine.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const USER32: &str = "USER32.dll";

/// The window handles that mean "the screen" rather than a window of the guest's:
/// no handle at all, and the desktop window.
const SCREEN: [u32; 2] = [0, 0x0001_0000];

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "CopyRect", arg_bytes: 8, shim: copy_rect },
    Binding { dll: USER32, name: "EqualRect", arg_bytes: 8, shim: equal_rect },
    Binding { dll: USER32, name: "InflateRect", arg_bytes: 12, shim: inflate_rect },
    Binding { dll: USER32, name: "IntersectRect", arg_bytes: 12, shim: intersect_rect },
    Binding { dll: USER32, name: "MapWindowPoints", arg_bytes: 16, shim: map_window_points },
    Binding { dll: USER32, name: "OffsetRect", arg_bytes: 12, shim: offset_rect },
    Binding { dll: USER32, name: "ReuseDDElParam", arg_bytes: 16, shim: reuse_dde_lparam },
    Binding { dll: USER32, name: "ScreenToClient", arg_bytes: 8, shim: screen_to_client },
    Binding { dll: USER32, name: "SetRectEmpty", arg_bytes: 4, shim: set_rect_empty },
    Binding { dll: USER32, name: "UnpackDDElParam", arg_bytes: 16, shim: unpack_dde_lparam },
];

/// `CopyRect(dst, src)`: the copy succeeds whenever both pointers exist.
fn copy_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let source = call.arg(1)?;
    if destination == 0 || source == 0 {
        return Ok(Return(0));
    }
    let rect = read_rect(call, source)?;
    write_rect(call, destination, rect)?;
    Ok(Return(1))
}

/// `EqualRect(a, b)`: whether the two rectangles are the same four numbers.
fn equal_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (first, second) = (call.arg(0)?, call.arg(1)?);
    if first == 0 || second == 0 {
        return Ok(Return(0));
    }
    Ok(Return(u32::from(
        read_rect(call, first)? == read_rect(call, second)?,
    )))
}

/// `InflateRect(lprc, dx, dy)`: grow the rectangle outwards by `dx` and `dy`,
/// which is how a guest makes room for a border or a focus ring.
fn inflate_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let rectangle = call.arg(0)?;
    let (dx, dy) = (call.arg(1)? as i32, call.arg(2)? as i32);
    if rectangle == 0 {
        return Ok(Return(0));
    }
    let [left, top, right, bottom] = read_rect(call, rectangle)?;
    write_rect(
        call,
        rectangle,
        [
            left.saturating_sub(dx),
            top.saturating_sub(dy),
            right.saturating_add(dx),
            bottom.saturating_add(dy),
        ],
    )?;
    Ok(Return(1))
}

/// `IntersectRect(dst, a, b)`: the overlap, or nothing.
///
/// Two rectangles that do not overlap leave `dst` untouched and answer zero, which
/// is what Windows does and what a guest's `if (IntersectRect(...))` tests.
fn intersect_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let (first, second) = (call.arg(1)?, call.arg(2)?);
    if destination == 0 || first == 0 || second == 0 {
        return Ok(Return(0));
    }
    let [a_left, a_top, a_right, a_bottom] = read_rect(call, first)?;
    let [b_left, b_top, b_right, b_bottom] = read_rect(call, second)?;
    let overlap = [
        a_left.max(b_left),
        a_top.max(b_top),
        a_right.min(b_right),
        a_bottom.min(b_bottom),
    ];
    if overlap[2] <= overlap[0] || overlap[3] <= overlap[1] {
        return Ok(Return(0));
    }
    write_rect(call, destination, overlap)?;
    Ok(Return(1))
}

/// `OffsetRect(lprc, dx, dy)`: move the rectangle in place.
fn offset_rect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let rectangle = call.arg(0)?;
    let (dx, dy) = (call.arg(1)? as i32, call.arg(2)? as i32);
    if rectangle == 0 {
        return Ok(Return(0));
    }
    let [left, top, right, bottom] = read_rect(call, rectangle)?;
    write_rect(
        call,
        rectangle,
        [
            left.saturating_add(dx),
            top.saturating_add(dy),
            right.saturating_add(dx),
            bottom.saturating_add(dy),
        ],
    )?;
    Ok(Return(1))
}

/// `SetRectEmpty(lprc)`: all four fields zero, which is the empty rectangle a
/// guest accumulates an extent into.
fn set_rect_empty(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let rectangle = call.arg(0)?;
    if rectangle == 0 {
        return Ok(Return(0));
    }
    write_rect(call, rectangle, [0, 0, 0, 0])?;
    Ok(Return(1))
}

/// `MapWindowPoints(from, to, points, count)`: convert points from one window's
/// client space to another's.
///
/// A handle of zero or the desktop means screen space, so a guest converting
/// between its window and the screen needs no special case. A count of zero means
/// the structure is a `RECT` and both of its corners are converted, which is the
/// documented shortcut a layout routine uses.
fn map_window_points(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let from = call.arg(0)?;
    let to = call.arg(1)?;
    let points = call.arg(2)?;
    let count = call.arg(3)? as i32;
    if points == 0 {
        return Ok(Return(0));
    }
    let (from_x, from_y) = origin_of(host, from);
    let (to_x, to_y) = origin_of(host, to);
    let (dx, dy) = (to_x - from_x, to_y - from_y);
    // A `RECT` has two points in it, which is what a zero count means.
    let corners = if count == 0 { 2 } else { count.max(0) };
    for corner in 0..corners {
        let at = points + corner as u32 * 8;
        let x = call.memory.read_u32(at)? as i32;
        let y = call.memory.read_u32(at + 4)? as i32;
        call.write_u32(at, x.saturating_add(dx) as u32)?;
        call.write_u32(at + 4, y.saturating_add(dy) as u32)?;
    }
    // Windows answers the offset it applied, x in the low word and y in the high;
    // a guest that only converts ignores it, and one that reads it gets the same
    // number Windows would have given.
    Ok(Return(
        ((dx as u32) & 0xffff) | ((((dy as u32) & 0xffff)) << 16),
    ))
}

/// `ScreenToClient(hwnd, lpPoint)`: screen coordinates to the window's client
/// space, which is where its drawing is measured from.
fn screen_to_client(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let point = call.arg(1)?;
    if point == 0 {
        return Ok(Return(0));
    }
    let (x, y) = origin_of(host, window);
    let screen_x = call.memory.read_u32(point)? as i32;
    let screen_y = call.memory.read_u32(point + 4)? as i32;
    call.write_u32(point, screen_x.saturating_sub(x) as u32)?;
    call.write_u32(point + 4, screen_y.saturating_sub(y) as u32)?;
    Ok(Return(1))
}

/// `ReuseDDElParam(msg, lParamIn, uiLo, uiHi)`: the two values packed into one
/// parameter, which is all a guest can observe of it here.
///
/// This host has no DDE conversation to lend a parameter block to, so the packing
/// that Windows does for a message with room in its `lParam` is the whole
/// behaviour.
fn reuse_dde_lparam(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let low = call.arg(2)? & 0x0000_ffff;
    let high = call.arg(3)? & 0x0000_ffff;
    Ok(Return(low | (high << 16)))
}

/// `UnpackDDElParam(msg, lParam, puiLo, puiHi)`: the halves back out of one.
fn unpack_dde_lparam(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parameter = call.arg(1)?;
    let low = call.arg(2)?;
    let high = call.arg(3)?;
    if low == 0 || high == 0 {
        return Ok(Return(0));
    }
    call.write_u32(low, parameter & 0x0000_ffff)?;
    call.write_u32(high, parameter >> 16)?;
    Ok(Return(1))
}

/// The screen position of a window's client origin: its own top-left, or the
/// screen itself for the two handles that mean one.
fn origin_of(host: &Host, window: u32) -> (i32, i32) {
    if SCREEN.contains(&window) {
        return (0, 0);
    }
    host.windows
        .window_rect(window)
        .map_or((0, 0), |rect| (rect[0], rect[1]))
}

/// A `RECT` from naive guest memory: `left, top, right, bottom`, four `LONG`s.
fn read_rect(call: &Call<'_>, at: u32) -> Result<[i32; 4]> {
    let mut rect = [0_i32; 4];
    for (index, value) in rect.iter_mut().enumerate() {
        *value = call.memory.read_u32(at + index as u32 * 4)? as i32;
    }
    Ok(rect)
}

fn write_rect(call: &mut Call<'_>, at: u32, rect: [i32; 4]) -> Result<()> {
    for (index, value) in rect.iter().enumerate() {
        call.write_u32(at + index as u32 * 4, *value as u32)?;
    }
    Ok(())
}
