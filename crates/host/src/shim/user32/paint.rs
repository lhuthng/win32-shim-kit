//! The `USER32.dll` calls that put pixels on the guest's window: the paint
//! begin/end pair, the rectangle and text fills, and the focus ring.
//!
//! None of these draw anything themselves. GDI owns the glyph rasteriser and the
//! palette conversion, and a guest measures a string with one call and draws it
//! with another -- so every fill and every run of text here goes through the same
//! primitives `gdi32` exports ([`crate::shim::gdi32::paint_rect`] and
//! [`crate::shim::gdi32::draw_run`]) and the line a guest laid out is the line it
//! gets. That also means a fill or a `DrawTextA` into the surface the page shows
//! marks the frame as changed, so the picture follows the guest's drawing.
//!
//! `BeginPaint` answers GDI's own window context, so a guest may select a font or
//! a bitmap into it and blit, exactly as it would on Windows. The paint struct's
//! `rcPaint` is the whole client area: this host has no accumulated update region
//! to hold it down to, which is also why `ExcludeUpdateRgn` has nothing to do.

use std::sync::atomic::{AtomicBool, Ordering};

use crate::shim::{gdi32, Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const USER32: &str = "USER32.dll";

/// The stock-object handle space GDI hands out (`0x00020000 + index * 4`), which
/// is what a brush handle looks like whether it came from `GetStockObject` or
/// `GetSysColorBrush`.
const STOCK_BASE: u32 = 0x0002_0000;

/// Where GDI's own `CreateSolidBrush` handles start. Their colours live in GDI's
/// brush table, which is not readable from here -- see [`brush_colour`].
const CREATED_BRUSH: u32 = 0x0002_0200;

/// Whether the notice about a created brush has been said. One per session: a
/// dialog repaints its background on every paint, and the fact does not change.
static BRUSH_NOTICE: AtomicBool = AtomicBool::new(false);

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "BeginPaint", arg_bytes: 8, shim: begin_paint },
    Binding { dll: USER32, name: "EndPaint", arg_bytes: 8, shim: end_paint },
    Binding { dll: USER32, name: "DrawFocusRect", arg_bytes: 8, shim: draw_focus_rect },
    Binding { dll: USER32, name: "DrawTextA", arg_bytes: 20, shim: draw_text },
    Binding { dll: USER32, name: "ExcludeUpdateRgn", arg_bytes: 8, shim: exclude_update_rgn },
    Binding { dll: USER32, name: "FillRect", arg_bytes: 12, shim: fill_rect },
    Binding { dll: USER32, name: "GrayStringA", arg_bytes: 36, shim: gray_string },
    Binding { dll: USER32, name: "TabbedTextOutA", arg_bytes: 32, shim: tabbed_text_out },
];

/// `BeginPaint(hwnd, lpPaint)`: the context to draw the window with, and the area
/// that needs painting.
///
/// `PAINTSTRUCT` is 64 bytes: the context at 0, `fErase` at 4, `rcPaint` at 8,
/// `fRestore` at 24, `fIncUpdate` at 28, and 32 reserved bytes at 32. Nothing is
/// erased here, so `fErase` says the background is the guest's to draw -- which
/// it is, because the guest's own surface is the display.
fn begin_paint(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let window = call.arg(0)?;
    let destination = call.arg(1)?;
    // The context draws on the screen, so the screen has to exist before a dialog
    // that paints itself through GDI asks for one.
    gdi32::ensure_window_screen(host)?;
    let dc = host.gdi.window_dc();
    let (screen_width, screen_height) = host.windows.screen();
    let rect = host
        .windows
        .client_rect(window)
        .unwrap_or([0, 0, screen_width, screen_height]);
    if destination != 0 {
        let mut paint = vec![0_u8; 64];
        paint[0..4].copy_from_slice(&dc.to_le_bytes());
        for (index, value) in rect.iter().enumerate() {
            let at = 8 + index * 4;
            paint[at..at + 4].copy_from_slice(&value.to_le_bytes());
        }
        call.write(destination, &paint)?;
    }
    Ok(Return(dc))
}

/// `EndPaint(hwnd, lpPaint)`: nothing to flush -- every write already landed in
/// the surface the page presents.
fn end_paint(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `ExcludeUpdateRgn(hdc, hwnd)`: there is no accumulated update region to
/// subtract from, because a paint here is the guest drawing its own frame.
fn exclude_update_rgn(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `DrawFocusRect(hdc, lprc)`: the focus ring, which Windows draws by inverting
/// what is already there so a second call erases it.
fn draw_focus_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let rectangle = call.arg(1)?;
    if rectangle == 0 {
        return Ok(Return(0));
    }
    let rect = read_rect(call, rectangle)?;
    gdi32::invert_rect(host, call.memory, dc, rect)?;
    Ok(Return(1))
}

/// `FillRect(hdc, lprc, hbr)`: paint the rectangle with the brush's colour.
///
/// The colour is resolved from the handle by [`brush_colour`]; a null brush fills
/// nothing and still succeeds, as it does on Windows.
fn fill_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let rectangle = call.arg(1)?;
    let brush = call.arg(2)?;
    if rectangle == 0 {
        return Ok(Return(0));
    }
    let rect = read_rect(call, rectangle)?;
    let Some(colour) = brush_colour(brush) else {
        // A null brush fills nothing and still succeeds; a brush this host cannot
        // read a colour out of is the case worth a word in the log.
        if brush >= CREATED_BRUSH && !BRUSH_NOTICE.swap(true, Ordering::Relaxed) {
            host.notice(format!(
                "FillRect with brush {brush:#010x} from CreateSolidBrush: its colour is private \
                 to GDI, so the rectangle is left unpainted"
            ));
        }
        return Ok(Return(u32::from(brush == 0)));
    };
    gdi32::paint_rect(host, call.memory, dc, rect, colour)?;
    Ok(Return(1))
}

/// The `COLORREF` a brush handle fills with.
///
/// Both `GetStockObject` and `GetSysColorBrush` answer a handle in GDI's stock
/// space, so one table serves both: indices 0 to 4 are the five stock brushes a
/// fill actually uses -- white, light grey, grey, dark grey, black -- and
/// everything above them is read as a `COLOR_*` index, because the stock objects
/// at those indices are pens, fonts and palettes rather than brushes. The overlap
/// is the only reading that makes `FillRect(hdc, &rc, GetSysColorBrush(COLOR_BTNFACE))`
/// grey, which is what every dialog of this era does.
///
/// A brush built by `CreateSolidBrush` is the one case this cannot answer: its
/// colour is in GDI's brush table, and a `USER32` shim has no way to read it. It
/// is reported once and left unpainted rather than painted black.
fn brush_colour(brush: u32) -> Option<u32> {
    if brush == 0 {
        // `NULL_BRUSH`: nothing to fill with, and the call still succeeds.
        return None;
    }
    if brush >= CREATED_BRUSH {
        return None;
    }
    let index = (brush.wrapping_sub(STOCK_BASE)) / 4;
    Some(match index {
        // The stock brushes.
        0 => 0x00ff_ffff,
        1 | 22 => 0x00c0_c0c0,
        2 | 12 | 16 | 17 => 0x0080_8080,
        3 | 21 => 0x0040_4040,
        // The `COLOR_*` entries a fill asks for.
        5 | 9 | 14 | 20 | 23 | 24 => 0x00ff_ffff,
        6 | 18 => 0x0000_0000,
        13 => 0x006a_240a,
        25 => 0x00ff_0000,
        26 => 0x00f0_caa6,
        27 => 0x00c0_c0c0,
        // `COLOR_SCROLLBAR`, `MENU`, `BTNFACE`, `ACTIVEBORDER` and the panels
        // around them: the 3D face every dialog of this era is painted with.
        _ => 0x00c8_d0d4,
    })
}

/// `DrawTextA(hdc, str, count, lprc, format)`: lay the string out in the
/// rectangle and draw the lines.
///
/// The flags that change what a player sees are handled: `DT_SINGLELINE` and the
/// horizontal and vertical alignment, `DT_WORDBREAK` with the width it measures
/// through GDI's own advances, `DT_EXPANDTABS` and `DT_CALCRECT` (write the
/// rectangle the text needs and draw nothing). A `&` marks the next character as
/// an accelerator and is not itself drawn, which is what a dialog's label does.
fn draw_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const DT_CENTER: u32 = 0x0000_0001;
    const DT_RIGHT: u32 = 0x0000_0002;
    const DT_VCENTER: u32 = 0x0000_0004;
    const DT_SINGLELINE: u32 = 0x0000_0020;
    const DT_EXPANDTABS: u32 = 0x0000_0040;
    const DT_CALCRECT: u32 = 0x0000_0400;

    let dc = call.arg(0)?;
    let string = call.arg(1)?;
    let count = call.arg(2)? as i32;
    let rectangle = call.arg(3)?;
    let format = call.arg(4)?;
    if string == 0 || rectangle == 0 {
        return Ok(Return(0));
    }
    if gdi32::device_of(host, dc).is_err() {
        return Ok(Return(0));
    }
    let raw = read_text(call, string, count)?;
    let text = strip_accelerators(&raw);
    let text = if format & DT_EXPANDTABS != 0 {
        expand_tabs(host, dc, &text)
    } else {
        text
    };
    let [left, top, right, bottom] = read_rect(call, rectangle)?;
    // The cell height is what a line steps by, and a context's font is the only
    // thing that decides it.
    let line_height = gdi32::text_extent(host, dc, "").1.max(1);
    let lines = wrap_text(host, dc, &text, right - left, format);
    let widest = lines
        .iter()
        .map(|line| gdi32::text_extent(host, dc, line).0)
        .max()
        .unwrap_or(0);
    let total = lines.len().max(1) as i32 * line_height;

    if format & DT_CALCRECT != 0 {
        // Nothing is drawn: the guest asked what the text needs, so the rectangle
        // it passed comes back holding exactly that.
        write_rect(call, rectangle, [left, top, left + widest, top + total])?;
        return Ok(Return(total as u32));
    }

    let mut y = top;
    if format & DT_VCENTER != 0 && format & DT_SINGLELINE != 0 {
        y = top + ((bottom - top) - line_height) / 2;
    }
    for line in &lines {
        let width = gdi32::text_extent(host, dc, line).0;
        let x = if format & DT_CENTER != 0 {
            left + ((right - left) - width) / 2
        } else if format & DT_RIGHT != 0 {
            right - width
        } else {
            left
        };
        gdi32::draw_run(host, call.memory, dc, x, y, line)?;
        y += line_height;
    }
    Ok(Return(total as u32))
}

/// `TabbedTextOutA(hdc, x, y, str, count, nTabs, tabStops, tabOrigin)`: draw the
/// string with its tabs moved to the caller's stops.
///
/// The answer is where the text ended, packed as Windows packs it: the x after the
/// last character in the low word and the y in the high one, which is what a guest
/// that draws a second column reads.
fn tabbed_text_out(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let string = call.arg(3)?;
    let count = call.arg(4)? as i32;
    let tabs = call.arg(5)?;
    let tab_stops = call.arg(6)?;
    let tab_origin = call.arg(7)? as i32;
    if string == 0 || gdi32::device_of(host, dc).is_err() {
        return Ok(Return(((x as u32) & 0xffff) | (((y as u32) & 0xffff) << 16)));
    }
    let text = read_text(call, string, count)?;
    // Without a stop list, a tab is eight ordinary advances, which is the default
    // every caller of this host relies on.
    let space = gdi32::text_extent(host, dc, " ").0.max(1);
    let stops: Vec<i32> = if tabs > 0 && tab_stops != 0 {
        (0..tabs)
            .map(|index| call.memory.read_u32(tab_stops + index * 4).map(|value| value as i32))
            .collect::<Result<Vec<i32>>>()?
    } else {
        Vec::new()
    };

    let mut cursor = x;
    let mut piece = String::new();
    for character in text.chars() {
        if character != '\t' {
            piece.push(character);
            continue;
        }
        if !piece.is_empty() {
            gdi32::draw_run(host, call.memory, dc, cursor, y, &piece)?;
            cursor += gdi32::text_extent(host, dc, &piece).0;
            piece.clear();
        }
        // The next stop after where the line has reached. The stops are measured
        // from `tabOrigin`, not from the draw x, as Windows measures them.
        let reached = cursor - tab_origin;
        cursor = if stops.is_empty() {
            let stop = (space * 8).max(1);
            tab_origin + (reached / stop + 1) * stop
        } else {
            tab_origin
                + stops
                    .iter()
                    .find(|stop| **stop > reached)
                    .copied()
                    .unwrap_or_else(|| stops.last().copied().unwrap_or(reached) + space * 8)
        };
    }
    if !piece.is_empty() {
        gdi32::draw_run(host, call.memory, dc, cursor, y, &piece)?;
        cursor += gdi32::text_extent(host, dc, &piece).0;
    }
    Ok(Return(((cursor as u32) & 0xffff) | (((y as u32) & 0xffff) << 16)))
}

/// `GrayStringA(hdc, hbr, lpOutputFunc, lpData, count, x, y, w, h)`: draw disabled
/// text.
///
/// With no output function the data is the string itself, which is the case every
/// dialog takes; it is drawn at `x, y` as it stands rather than dithered, because
/// this host has one ink per pixel and no grey to blend with.
fn gray_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let output = call.arg(2)?;
    let data = call.arg(3)?;
    let count = call.arg(4)? as i32;
    let (x, y) = (call.arg(5)? as i32, call.arg(6)? as i32);
    if output != 0 {
        // The guest asked this host to call back into it from inside a draw. A
        // shim cannot: it runs inside the interpreter with the guest's own frame
        // suspended underneath, so the routine would have to be run by the
        // session after this call returns -- and the text would be drawn after
        // the fill that was meant to sit under it.
        host.notice(format!(
            "GrayStringA with an output function at {output:#010x}: this host cannot run a guest \
             callback from inside a draw, so the text is not drawn"
        ));
        return Ok(Return(0));
    }
    if data == 0 || count == 0 || gdi32::device_of(host, dc).is_err() {
        return Ok(Return(1));
    }
    let text = read_text(call, data, count)?;
    gdi32::draw_run(host, call.memory, dc, x, y, &text)?;
    Ok(Return(1))
}

// ---- reading what the guest passed ----------------------------------------

/// The text at a pointer, honouring the negative count that means
/// "NUL-terminated", which is how every one of these calls may be written.
fn read_text(call: &Call<'_>, at: u32, count: i32) -> Result<String> {
    if count < 0 {
        return call.memory.read_cstr(at);
    }
    let bytes = call.memory.read_bytes(at, count as usize)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// The `&` accelerator marker removed: `&File` is drawn `File`, and `&&` is one
/// literal `&`. The underline is not modelled -- one ink per pixel means a
/// character cannot be drawn both plain and underlined.
fn strip_accelerators(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '&' {
            out.push(character);
            continue;
        }
        match characters.peek() {
            Some('&') => {
                characters.next();
                out.push('&');
            }
            // The next character is the accelerator: drawn plain, not underlined.
            Some(_) => {}
            None => out.push('&'),
        }
    }
    out
}

/// Tabs turned into spaces, which is what `DT_EXPANDTABS` asks for. The stop is
/// eight average advances, measured as the guest's font draws them.
fn expand_tabs(host: &mut Host, dc: u32, text: &str) -> String {
    let space = gdi32::text_extent(host, dc, " ").0.max(1);
    let stop = (space * 8).max(1);
    let mut out = String::with_capacity(text.len() + 8);
    let mut cursor = 0;
    for character in text.chars() {
        if character == '\t' {
            let target = (cursor / stop + 1) * stop;
            while cursor < target {
                out.push(' ');
                cursor += space;
            }
            continue;
        }
        let mut buffer = [0_u8; 4];
        out.push(character);
        cursor += gdi32::text_extent(host, dc, character.encode_utf8(&mut buffer)).0;
    }
    out
}

/// The string broken into the lines `DrawTextA` will draw: one per explicit break,
/// and, when `DT_WORDBREAK` is set and the line is not a single one, as many as the
/// width needs.
///
/// A word wider than the rectangle is left on a line of its own rather than broken
/// between characters: Windows breaks a word only with `DT_EDITCONTROL` or an edit
/// control's own layout, and a guest that measured the word expects it whole.
fn wrap_text(host: &mut Host, dc: u32, text: &str, width: i32, format: u32) -> Vec<String> {
    const DT_WORDBREAK: u32 = 0x0000_0010;
    const DT_SINGLELINE: u32 = 0x0000_0020;

    let single = format & DT_SINGLELINE != 0;
    let word_break = format & DT_WORDBREAK != 0;
    let normalised = text.replace("\r\n", "\n");
    let mut lines = Vec::new();
    for paragraph in normalised.split('\n') {
        if single || !word_break || width <= 0 {
            lines.push(paragraph.to_string());
            continue;
        }
        let mut line = String::new();
        for word in paragraph.split(' ') {
            let candidate = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && gdi32::text_extent(host, dc, &candidate).0 > width {
                lines.push(std::mem::take(&mut line));
                line.push_str(word);
            } else {
                line = candidate;
            }
        }
        lines.push(line);
    }
    lines
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
