//! The `GDI32.dll` subset a boot needs: stock objects, the bitmap path, and the
//! text a game draws with a device context.
//!
//! Three things are modelled for real, because a guest reads values back out of
//! them or because they are what the player sees:
//!
//! - **Bitmaps** the image carries as resources. `LoadImageA` registers the
//!   `BITMAPINFOHEADER` and the pixels' address, and `GetObjectA` reports the
//!   size the guest asks for.
//! - **Device contexts** and `StretchBlt`, so a guest can take such a bitmap,
//!   select it into a DC and stretch it into a DirectDraw surface's DC. Cave
//!   Story loads its credits screens that way.
//! - **Text**: `CreateFontA` and `TextOutA` draw the glyphs into the surface the
//!   context belongs to, through its palette. Cave Story's credits ask a surface
//!   for a context, select the font, set the background mode and the ink, and
//!   call `TextOutA`; that comes out as a picture rather than a stop.
//!
//! A stretch blit maps colours through the two palettes -- the DIB's own and the
//! DirectDraw palette the destination surface draws with -- since the destination
//! is an 8-bit palettised surface. Splitting a source pixel into an RGB triple
//! and finding the nearest entry is what GDI does with `SRCCOPY` between two
//! palettised devices.
//!
//! A guest can own pixels without a DirectDraw surface too: `CreateDIBSection`
//! allocates them out of the arena, copies the colour table out of the
//! `BITMAPINFO` it was handed, and gives the address back, so the guest writes
//! the same bytes GDI reads. Menu artwork is built that way and then put on the
//! screen with `BitBlt`, `StretchBlt` or a `PatBlt` fill, which is why a blit
//! reads each side as a *device* -- a surface's context or a bitmap selected into
//! one -- rather than assuming a surface underneath. The context `GetDC` answers
//! for the window is one of these too, drawing on whichever primary surface the
//! guest has when it draws, so what a menu blits through it is what the page
//! shows.
//!
//! Of the font a guest asks for, one field is kept -- the cell height, which
//! decides how large the glyphs are drawn. The face is this host's own 8x20 ASCII
//! face, not the system's: there are no system fonts to load, so a guest that
//! asks for "Arial" or "Courier New" gets the pixels this file carries. It is
//! drawn at the size Courier New would be -- twelve pixels of advance at
//! twenty pixels of height -- so a guest's line layout is the same as the layout
//! it computed for the font it asked for. That is a substitution a player can see,
//! so it is said once per session, and a character the face does not have is drawn
//! as a box rather than dropped.

use std::collections::HashMap;

use crate::memory::GuestMemory;
use crate::shim::{Action, Binding, Call, Host};

/// Whether the GDI trace is on. A blit asks this on every call, and a guest draws
/// its frame out of thousands of them, so it is read once.
static LOGGING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
use crate::Result;

use Action::Return;

const GDI32: &str = "GDI32.dll";

const FIRST_OBJECT: u32 = 0x0002_0000;

/// `CreateFontA`'s handle: one font, distinct from the stock objects.
const FONT_HANDLE: u32 = FIRST_OBJECT + 0x100;

/// `CreateSolidBrush`'s handles, above the fonts so a log line can say which
/// kind of object a number is.
const FIRST_BRUSH: u32 = FIRST_OBJECT + 0x200;

/// The objects Windows starts a fresh context with, which are the answer when a
/// guest selects the first object of a kind: the stock white brush, the default
/// GUI font, and the 1x1 monochrome bitmap every memory context holds. A guest
/// checks that answer for null, so zero would read as the selection failing.
const STOCK_BRUSH: u32 = FIRST_OBJECT;
const STOCK_BITMAP: u32 = FIRST_OBJECT + 0x40;
const STOCK_FONT: u32 = FIRST_OBJECT + 17 * 4;

/// Device-context handles, above the object handles so a log line can say which
/// of the two a number is.
const FIRST_DC_HANDLE: u32 = 0x0003_0000;

/// Bitmap handles for files `LoadImageA` loaded, above the resource ones, so the
/// two kinds do not collide in a log line.
const FIRST_BITMAP: u32 = 0x0001_0500;

/// The highest stock index on 32-bit Windows (`DC_PEN`); anything above it is
/// not a stock object and answers null, as it does on Windows.
const LAST_STOCK_INDEX: u32 = 19;

/// `BITMAPINFOHEADER`: the header size this host can read, and where its fields
/// sit.
const BITMAPINFOHEADER: u32 = 40;
const BI_RGB: u32 = 0;

/// `SRCCOPY`: the only raster operation a blit is asked for here.
const SRCCOPY: u32 = 0x00cc_0020;

/// The raster operations `PatBlt` fills with: the selected brush, all white, all
/// black, and the inverse of what is already there.
const PATCOPY: u32 = 0x00f0_0021;
const WHITENESS: u32 = 0x00ff_0062;
const BLACKNESS: u32 = 0x0000_0042;
const DSTINVERT: u32 = 0x0055_0009;

/// The built-in face's cell: twelve pixels of advance, of which two are the left
/// bearing, eight are the glyph's own columns and two are letter spacing. Twenty
/// rows are cap height plus descenders, the size Courier New would be drawn at
/// when a guest asks for a twenty-pixel font.
const ADVANCE: i32 = 12;
const GLYPH_LEFT: i32 = 2;
const GLYPH_WIDTH: i32 = 8;
const GLYPH_HEIGHT: i32 = 20;

pub const SHIMS: &[Binding] = &[
    Binding { dll: GDI32, name: "GetStockObject", arg_bytes: 4, shim: get_stock_object },
    Binding { dll: GDI32, name: "CreateFontA", arg_bytes: 56, shim: create_font },
    Binding { dll: GDI32, name: "CreateCompatibleDC", arg_bytes: 4, shim: create_compatible_dc },
    Binding { dll: GDI32, name: "SelectObject", arg_bytes: 8, shim: select_object },
    Binding { dll: GDI32, name: "DeleteObject", arg_bytes: 4, shim: delete_object },
    Binding { dll: GDI32, name: "DeleteDC", arg_bytes: 4, shim: delete_dc },
    Binding { dll: GDI32, name: "GetObjectA", arg_bytes: 12, shim: get_object },
    Binding { dll: GDI32, name: "StretchBlt", arg_bytes: 44, shim: stretch_blt },
    Binding { dll: GDI32, name: "GetPixel", arg_bytes: 12, shim: get_pixel },
    Binding { dll: GDI32, name: "SetPixel", arg_bytes: 16, shim: set_pixel },
    Binding { dll: GDI32, name: "SetBkMode", arg_bytes: 8, shim: set_bk_mode },
    Binding { dll: GDI32, name: "TextOutA", arg_bytes: 20, shim: text_out },
    Binding { dll: GDI32, name: "SetTextColor", arg_bytes: 8, shim: set_text_color },
    Binding { dll: GDI32, name: "SetBkColor", arg_bytes: 8, shim: set_bk_color },
    Binding { dll: GDI32, name: "GetObjectW", arg_bytes: 12, shim: get_object_w },
    Binding { dll: GDI32, name: "TextOutW", arg_bytes: 20, shim: text_out_w },
    Binding { dll: GDI32, name: "CreateDIBSection", arg_bytes: 24, shim: create_dib_section },
    Binding { dll: GDI32, name: "CreateFontW", arg_bytes: 56, shim: create_font_w },
    Binding { dll: GDI32, name: "CreateSolidBrush", arg_bytes: 4, shim: create_solid_brush },
    Binding { dll: GDI32, name: "BitBlt", arg_bytes: 36, shim: bit_blt },
    Binding { dll: GDI32, name: "PatBlt", arg_bytes: 24, shim: pat_blt },
];

/// `TextOutA(dc, x, y, string, count)`: draws the glyphs of `string` at `x, y`.
///
/// The top-left corner is where the first glyph's cell starts, which is what a
/// guest computes its line layout against. A negative `count` means the string is
/// NUL-terminated, as it does on Windows.
///
/// The face this host has is its own 8x20 ASCII font, drawn to Courier New's own
/// cell so that the line widths a guest computed for the font it asked for are the
/// widths this draws. Cave Story asks for a twenty-pixel Courier New and then draws
/// plain ASCII through it; a guest that asks for Japanese would get boxes, and the
/// notice says so once.
fn text_out(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = call.arg(3)?;
    let count = call.arg(4)? as i32;
    let text = if count < 0 {
        call.memory.read_cstr(string)?
    } else {
        let bytes = call.memory.read_bytes(string, count as usize)?;
        String::from_utf8_lossy(bytes).into_owned()
    };
    draw_text(host, call, "TextOutA", text)
}

/// `TextOutW`: the same face, with UTF-16 text.
///
/// The built-in face holds ASCII; a character it does not have is drawn as `?`,
/// which is what a Windows font without those glyphs draws too.
fn text_out_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = call.arg(3)?;
    let count = call.arg(4)? as i32;
    let text = if count < 0 {
        call.wstr(string)?
    } else {
        let mut units = Vec::with_capacity(count as usize);
        for index in 0..count as u32 {
            units.push(call.memory.read_u16(string + index * 2)?);
        }
        String::from_utf16_lossy(&units)
    };
    draw_text(host, call, "TextOutW", text)
}

/// The drawing both text calls share, once their text is decoded.
fn draw_text(host: &mut Host, call: &mut Call<'_>, method: &str, text: String) -> Result<Action> {
    let dc = call.arg(0)?;
    let x = call.arg(1)? as i32;
    let y = call.arg(2)? as i32;
    if std::env::var("SHIMKIT_LOG_GDI").is_ok() && host.notices.len() < 600 {
        host.notice(format!("{method} at {x},{y} into {dc:#x}: {text:?}"));
    }

    let Some(context) = host.gdi.dcs.get(&dc) else {
        return Err(format!("{method} into DC {dc:#010x}, which does not exist"));
    };
    let (foreground, background, opaque, scale) = {
        let text = context.text_color;
        let fill = context.background_color;
        let opaque = context.background == 2;
        let height = context
            .font
            .and_then(|font| host.gdi.fonts.get(&font))
            .map_or(GLYPH_HEIGHT, |font| font.height.unsigned_abs() as i32);
        // The built-in face is twenty pixels tall including its descender rows,
        // and a guest's `lfHeight` is the cell it wants: at twenty the glyphs are
        // drawn as designed, and above that they are doubled, because a
        // fractional stretch would break their one-pixel strokes.
        let scale = ((height + 10) / GLYPH_HEIGHT).clamp(1, 2);
        (colour_of(text), colour_of(fill), opaque, scale)
    };

    if !host.gdi.font_notice {
        host.gdi.font_notice = true;
        let face = host
            .gdi
            .dcs
            .get(&dc)
            .and_then(|context| context.font)
            .and_then(|font| host.gdi.fonts.get(&font))
            .map_or_else(|| "the current font".to_string(), |font| format!("\"{}\"", font.face));
        host.notice(format!(
            "{method}: this host has no system fonts, so GDI text is drawn with its built-in 8x20 ASCII face (the guest selected {face})"
        ));
    }

    // The context's own device: a surface for a game drawing straight to the
    // screen, a bitmap for one laying its text out off screen to blit up later.
    let mut painter = Painter::new(device_of(host, dc)?);

    // The face is fixed pitch, so a string's width is its length times this,
    // whatever it says -- which is what a guest's line layout was computed
    // against when it chose the font.
    let advance = ADVANCE * scale;
    let mut cursor = x;
    for character in text.chars() {
        let glyph = glyph(character);
        if opaque {
            for row in 0..GLYPH_HEIGHT * scale {
                for column in 0..advance {
                    painter.put(call.memory, cursor + column, y + row, background)?;
                }
            }
        }
        for (row, bits) in glyph.iter().enumerate() {
            for column in 0..GLYPH_WIDTH {
                // Bit 7 is the leftmost column of the row.
                if bits & (0x80 >> column) == 0 {
                    continue;
                }
                for dy in 0..scale {
                    for dx in 0..scale {
                        painter.put(
                            call.memory,
                            cursor + (GLYPH_LEFT + column) * scale + dx,
                            y + row as i32 * scale + dy,
                            foreground,
                        )?;
                    }
                }
            }
        }
        cursor += advance;
    }
    // Text drawn into the surface the page shows is a frame like any other.
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(1))
}

/// `SetBkMode(dc, mode)`: the previous mode.
fn set_bk_mode(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let mode = call.arg(1)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    Ok(Return(std::mem::replace(&mut context.background, mode)))
}

/// `SetBkColor(dc, colour)`: the previous background colour.
fn set_bk_color(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let colour = call.arg(1)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(CLR_INVALID));
    };
    Ok(Return(std::mem::replace(
        &mut context.background_color,
        colour,
    )))
}

/// `SetTextColor(dc, colour)`: the previous colour.
fn set_text_color(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let colour = call.arg(1)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(CLR_INVALID));
    };
    Ok(Return(std::mem::replace(&mut context.text_color, colour)))
}

/// A `COLORREF` split into red, green and blue.
fn colour_of(colour: u32) -> (u8, u8, u8) {
    (
        colour as u8,
        ((colour >> 8) & 0xff) as u8,
        ((colour >> 16) & 0xff) as u8,
    )
}

/// One glyph of the built-in face: eight columns of twenty rows, bit 7 leftmost.
fn glyph(character: char) -> [u8; 20] {
    let index = character as u32;
    if (0x20..0x7f).contains(&index) {
        FONT[(index - 0x20) as usize]
    } else {
        // Anything the face does not have is drawn as a hollow box, which is what
        // a rasteriser does rather than dropping the character.
        [
            0x00, 0x00, 0x00, 0xff, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xff,
            0x00, 0x00, 0x00, 0x00, 0x00,
        ]
    }
}

/// The built-in face: ASCII 0x20 to 0x7e, eight columns of ink in a twelve-pixel
/// advance and twenty rows tall -- Courier New's own cell at the size games ask
/// for, which is twenty pixels.
///
/// The rows are the design's bands: row 3 is cap height, rows 7 to 15 the
/// x-height, row 15 the baseline, and rows 16 to 19 the descenders. Bit 7 is the
/// leftmost of the eight columns; the two pixels of left bearing and the two of
/// letter spacing that complete the advance are not part of a glyph.
const FONT: [[u8; 20]; 95] = [
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], //  
    [0x00, 0x00, 0x00, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x00, 0x00, 0x10, 0x10, 0x00, 0x00, 0x00, 0x00], // !
    [0x00, 0x00, 0x00, 0x24, 0x24, 0x24, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // quote
    [0x00, 0x00, 0x00, 0x00, 0x24, 0x24, 0x24, 0xff, 0x24, 0xff, 0x24, 0x24, 0x24, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // #
    [0x00, 0x00, 0x00, 0x10, 0x7c, 0x92, 0x90, 0x7c, 0x12, 0x92, 0x7c, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // $
    [0x00, 0x00, 0x00, 0x62, 0x92, 0x92, 0x64, 0x08, 0x10, 0x20, 0x36, 0x49, 0x49, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // %
    [0x00, 0x00, 0x00, 0x30, 0x48, 0x48, 0x30, 0x4a, 0x8e, 0x84, 0x84, 0x46, 0x39, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // &
    [0x00, 0x00, 0x00, 0x10, 0x10, 0x10, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // apostrophe
    [0x00, 0x00, 0x04, 0x08, 0x10, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x10, 0x08, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00], // (
    [0x00, 0x00, 0x20, 0x10, 0x08, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x08, 0x10, 0x20, 0x00, 0x00, 0x00, 0x00, 0x00], // )
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x92, 0x44, 0x38, 0x44, 0x92, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00], // *
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x10, 0x10, 0xff, 0x10, 0x10, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00], // +
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x20, 0x40, 0x00, 0x00], // ,
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // -
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x30, 0x00, 0x00, 0x00], // .
    [0x00, 0x00, 0x02, 0x02, 0x04, 0x04, 0x08, 0x08, 0x10, 0x10, 0x20, 0x20, 0x40, 0x40, 0x80, 0x80, 0x00, 0x00, 0x00, 0x00], // /
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // 0
    [0x00, 0x00, 0x00, 0x10, 0x30, 0x50, 0x90, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x7e, 0x00, 0x00, 0x00, 0x00], // 1
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x01, 0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0xff, 0x00, 0x00, 0x00, 0x00], // 2
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x01, 0x01, 0x01, 0x1c, 0x01, 0x01, 0x01, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // 3
    [0x00, 0x00, 0x00, 0x02, 0x06, 0x0a, 0x12, 0x22, 0x42, 0x82, 0x82, 0xff, 0x02, 0x02, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00], // 4
    [0x00, 0x00, 0x00, 0xff, 0x80, 0x80, 0x80, 0x80, 0xf8, 0x02, 0x01, 0x01, 0x01, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // 5
    [0x00, 0x00, 0x00, 0x1c, 0x20, 0x40, 0x80, 0x80, 0x80, 0xfc, 0x82, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // 6
    [0x00, 0x00, 0x00, 0xff, 0x01, 0x02, 0x04, 0x08, 0x10, 0x10, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00], // 7
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // 8
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x41, 0x3f, 0x01, 0x01, 0x01, 0x01, 0x22, 0x1c, 0x00, 0x00, 0x00, 0x00], // 9
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00], // :
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x30, 0x20, 0x40, 0x00, 0x00, 0x00], // ;
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x04, 0x08, 0x10, 0x20, 0x10, 0x08, 0x04, 0x02, 0x00, 0x00, 0x00], // <
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x7e, 0x00, 0x7e, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // =
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x40, 0x20, 0x10, 0x08, 0x10, 0x20, 0x40, 0x80, 0x00, 0x00, 0x00], // >
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x01, 0x01, 0x02, 0x04, 0x08, 0x08, 0x00, 0x00, 0x08, 0x08, 0x00, 0x00, 0x00, 0x00], // ?
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x99, 0xa5, 0xa5, 0xa5, 0x99, 0x80, 0x80, 0x40, 0x3c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // @
    [0x00, 0x00, 0x00, 0x18, 0x24, 0x42, 0x42, 0x81, 0x81, 0x81, 0xff, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00, 0x00], // A
    [0x00, 0x00, 0x00, 0xfc, 0x82, 0x82, 0x82, 0xfc, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0x82, 0xfc, 0x00, 0x00, 0x00, 0x00], // B
    [0x00, 0x00, 0x00, 0x3e, 0x41, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x41, 0x3e, 0x00, 0x00, 0x00, 0x00], // C
    [0x00, 0x00, 0x00, 0xf8, 0x84, 0x82, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x82, 0x84, 0xf8, 0x00, 0x00, 0x00, 0x00], // D
    [0x00, 0x00, 0x00, 0xff, 0x80, 0x80, 0x80, 0x80, 0x80, 0xf8, 0x80, 0x80, 0x80, 0x80, 0x80, 0xff, 0x00, 0x00, 0x00, 0x00], // E
    [0x00, 0x00, 0x00, 0xff, 0x80, 0x80, 0x80, 0x80, 0xf8, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 0x00, 0x00, 0x00], // F
    [0x00, 0x00, 0x00, 0x3e, 0x41, 0x80, 0x80, 0x80, 0x80, 0x8f, 0x81, 0x81, 0x81, 0x81, 0x41, 0x3e, 0x00, 0x00, 0x00, 0x00], // G
    [0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0xff, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00, 0x00], // H
    [0x00, 0x00, 0x00, 0x7e, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x7e, 0x00, 0x00, 0x00, 0x00], // I
    [0x00, 0x00, 0x00, 0x3f, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x02, 0x82, 0x44, 0x38, 0x00, 0x00, 0x00, 0x00], // J
    [0x00, 0x00, 0x00, 0x81, 0x82, 0x84, 0x88, 0x90, 0xe0, 0x90, 0x88, 0x84, 0x82, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00, 0x00], // K
    [0x00, 0x00, 0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0xff, 0x00, 0x00, 0x00, 0x00], // L
    [0x00, 0x00, 0x00, 0x81, 0xc3, 0xc3, 0xa5, 0xa5, 0x95, 0x99, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00, 0x00], // M
    [0x00, 0x00, 0x00, 0x81, 0xc1, 0xa1, 0x91, 0x89, 0x85, 0x83, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00, 0x00], // N
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // O
    [0x00, 0x00, 0x00, 0xfc, 0x82, 0x82, 0x82, 0x82, 0xfc, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 0x00, 0x00, 0x00], // P
    [0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x89, 0x85, 0x42, 0x3d, 0x00, 0x00, 0x00, 0x00], // Q
    [0x00, 0x00, 0x00, 0xfc, 0x82, 0x82, 0x82, 0xfc, 0x88, 0x84, 0x82, 0x81, 0x81, 0x81, 0x81, 0x80, 0x00, 0x00, 0x00, 0x00], // R
    [0x00, 0x00, 0x00, 0x3e, 0x41, 0x80, 0x80, 0x40, 0x3c, 0x01, 0x01, 0x01, 0x01, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // S
    [0x00, 0x00, 0x00, 0xff, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00], // T
    [0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // U
    [0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x81, 0x66, 0x66, 0x42, 0x42, 0x24, 0x24, 0x24, 0x10, 0x10, 0x00, 0x00, 0x00, 0x00], // V
    [0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x81, 0x81, 0x89, 0x89, 0xaa, 0xaa, 0xaa, 0xc6, 0xc6, 0x82, 0x00, 0x00, 0x00, 0x00], // W
    [0x00, 0x00, 0x00, 0x81, 0x81, 0x42, 0x42, 0x24, 0x24, 0x18, 0x18, 0x24, 0x24, 0x42, 0x42, 0x81, 0x00, 0x00, 0x00, 0x00], // X
    [0x00, 0x00, 0x00, 0x81, 0x81, 0x42, 0x42, 0x24, 0x24, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x18, 0x00, 0x00, 0x00, 0x00], // Y
    [0x00, 0x00, 0x00, 0xff, 0x02, 0x04, 0x04, 0x08, 0x08, 0x10, 0x10, 0x20, 0x20, 0x40, 0x40, 0xff, 0x00, 0x00, 0x00, 0x00], // Z
    [0x00, 0x00, 0x7c, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x40, 0x7c, 0x00, 0x00, 0x00, 0x00], // [
    [0x00, 0x00, 0x80, 0x80, 0x40, 0x40, 0x20, 0x20, 0x10, 0x10, 0x08, 0x08, 0x04, 0x04, 0x02, 0x02, 0x00, 0x00, 0x00, 0x00], // backslash
    [0x00, 0x00, 0x7c, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x7c, 0x00, 0x00, 0x00, 0x00], // ]
    [0x00, 0x00, 0x00, 0x10, 0x28, 0x44, 0x82, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ^
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff], // _
    [0x00, 0x00, 0x00, 0x20, 0x10, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // `
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3e, 0x01, 0x01, 0x3f, 0x41, 0x41, 0x41, 0x3f, 0x00, 0x00, 0x00], // a
    [0x00, 0x00, 0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0xfc, 0x82, 0x81, 0x81, 0x81, 0x81, 0x82, 0xfc, 0x00, 0x00, 0x00, 0x00], // b
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3e, 0x41, 0x80, 0x80, 0x80, 0x80, 0x41, 0x3e, 0x00, 0x00, 0x00], // c
    [0x00, 0x00, 0x00, 0x02, 0x02, 0x02, 0x02, 0x02, 0x3f, 0x41, 0x81, 0x81, 0x81, 0x81, 0x41, 0x3f, 0x00, 0x00, 0x00, 0x00], // d
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0xff, 0x80, 0x80, 0x42, 0x3c, 0x00, 0x00, 0x00], // e
    [0x00, 0x00, 0x00, 0x1e, 0x21, 0x20, 0x20, 0x20, 0xfc, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00], // f
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0x41, 0x81, 0x81, 0x81, 0x41, 0x3f, 0x01, 0x82, 0x7c, 0x00], // g
    [0x00, 0x00, 0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0xf8, 0x84, 0x82, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00, 0x00], // h
    [0x00, 0x00, 0x00, 0x10, 0x10, 0x00, 0x00, 0x00, 0x30, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x38, 0x00, 0x00, 0x00, 0x00], // i
    [0x00, 0x00, 0x00, 0x08, 0x08, 0x00, 0x00, 0x00, 0x18, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x08, 0x88, 0x70], // j
    [0x00, 0x00, 0x00, 0x80, 0x80, 0x80, 0x80, 0x80, 0x84, 0x88, 0x90, 0xe0, 0x90, 0x88, 0x84, 0x82, 0x00, 0x00, 0x00, 0x00], // k
    [0x00, 0x00, 0x00, 0x30, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x38, 0x00, 0x00, 0x00, 0x00], // l
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfc, 0x84, 0x84, 0x84, 0x84, 0x84, 0x84, 0x84, 0x00, 0x00, 0x00], // m
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xf8, 0x84, 0x82, 0x81, 0x81, 0x81, 0x81, 0x81, 0x00, 0x00, 0x00], // n
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3c, 0x42, 0x81, 0x81, 0x81, 0x42, 0x3c, 0x00, 0x00, 0x00, 0x00], // o
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xfc, 0x82, 0x81, 0x81, 0x81, 0x82, 0xfc, 0x80, 0x80, 0x80, 0x80], // p
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3f, 0x41, 0x81, 0x81, 0x81, 0x41, 0x3f, 0x01, 0x01, 0x01, 0x01], // q
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xbc, 0xc2, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x00, 0x00, 0x00], // r
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x3e, 0x41, 0x80, 0x7c, 0x02, 0x02, 0x82, 0x7c, 0x00, 0x00, 0x00], // s
    [0x00, 0x00, 0x00, 0x20, 0x20, 0x20, 0x20, 0xfc, 0x20, 0x20, 0x20, 0x20, 0x20, 0x21, 0x1e, 0x00, 0x00, 0x00, 0x00, 0x00], // t
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x82, 0x7e, 0x00, 0x00, 0x00], // u
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x42, 0x42, 0x24, 0x18, 0x00, 0x00, 0x00, 0x00], // v
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x91, 0x91, 0x91, 0xd3, 0x46, 0x00, 0x00, 0x00], // w
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81, 0x42, 0x24, 0x18, 0x24, 0x42, 0x81, 0x00, 0x00, 0x00, 0x00], // x
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x81, 0x81, 0x81, 0x81, 0x81, 0x81, 0x41, 0x3f, 0x01, 0x82, 0x7c], // y
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x04, 0x08, 0x10, 0x20, 0x40, 0xff, 0x00, 0x00, 0x00, 0x00], // z
    [0x00, 0x00, 0x0e, 0x10, 0x10, 0x10, 0x10, 0x10, 0x20, 0x40, 0x20, 0x10, 0x10, 0x10, 0x10, 0x0e, 0x00, 0x00, 0x00, 0x00], // {
    [0x00, 0x00, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0x00], // |
    [0x00, 0x00, 0x70, 0x08, 0x08, 0x08, 0x08, 0x08, 0x04, 0x02, 0x04, 0x08, 0x08, 0x08, 0x08, 0x70, 0x00, 0x00, 0x00, 0x00], // }
    [0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x61, 0x92, 0x8c, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00], // ~
];

/// `SetPixel(dc, x, y, colour)`: writes one pixel and answers the colour it
/// actually stored, which on a palettised device is the nearest entry.
fn set_pixel(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let x = call.arg(1)? as i32;
    let y = call.arg(2)? as i32;
    let (red, green, blue) = colour_of(call.arg(3)?);
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(CLR_INVALID));
    };
    let mut painter = Painter::new(device);
    if !painter.holds(x, y) {
        return Ok(Return(CLR_INVALID));
    }
    painter.put(call.memory, x, y, (red, green, blue))?;
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    // Read the pixel back so the answer is what a palettised device really
    // holds, which is one of its own entries.
    let (red, green, blue) =
        read_pixel(call.memory, &painter.device, x, y)?.unwrap_or((red, green, blue));
    Ok(Return(colour_ref(red, green, blue)))
}

/// `CLR_INVALID`: the colour a point outside the device or an unknown DC answers.
const CLR_INVALID: u32 = 0xffff_ffff;

/// `GetPixel(dc, x, y)`: the colour of one pixel, as a `COLORREF` -- `0x00BBGGRR`
/// whatever the surface stores, because that is what the guest reads next.
///
/// Cave Story samples its pixel font's mask this way: the glyph bitmap is on a
/// surface, and the game asks the device context for each dot.
fn get_pixel(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let x = call.arg(1)? as i32;
    let y = call.arg(2)? as i32;
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(CLR_INVALID));
    };
    match read_pixel(call.memory, &device, x, y)? {
        Some((red, green, blue)) => Ok(Return(colour_ref(red, green, blue))),
        None => Ok(Return(CLR_INVALID)),
    }
}

/// `COLORREF`'s packing: blue in the high byte, then green, then red.
fn colour_ref(red: u8, green: u8, blue: u8) -> u32 {
    (u32::from(blue) << 16) | (u32::from(green) << 8) | u32::from(red)
}

/// The stride of a DIB's rows: its pixels padded out to a multiple of four
/// bytes, which is the layout Windows writes them with and the one this host
/// reads.
fn stride_of(width: i32, bits: u16) -> u32 {
    ((width.unsigned_abs() as i64 * i64::from(bits) + 31) / 32 * 4) as u32
}

/// The colour table at `at`, one BGRA entry per colour, as RGB triples.
fn read_colours(memory: &GuestMemory, at: u32, count: usize) -> Result<Vec<[u8; 3]>> {
    let mut colours = Vec::with_capacity(count);
    for index in 0..count as u32 {
        let entry = at + index * 4;
        colours.push([
            memory.read_u8(entry + 2)?,
            memory.read_u8(entry + 1)?,
            memory.read_u8(entry)?,
        ]);
    }
    Ok(colours)
}

/// A bitmap the image carries or GDI allocated, as a device-independent bitmap.
struct Bitmap {
    width: i32,
    /// Negative when the rows are stored top-down, which is the sign's meaning.
    height: i32,
    bits: u16,
    /// Guest address of the pixel data: past the header and the palette.
    pixels: u32,
    /// The colour table, copied out when the bitmap was registered so a guest may
    /// reuse or release the `BITMAPINFO` it described it with. Empty for a
    /// truecolour bitmap.
    colours: Vec<[u8; 3]>,
    /// The arena block holding the pixels when GDI allocated them
    /// (`CreateDIBSection`), which is what `DeleteObject` hands back.
    owned: Option<u32>,
}

/// A font the guest asked for. Only the pixel height matters to a rasteriser
/// that has one face, but the requested name is kept so the log can say which
/// one it is substituting for.
struct Font {
    height: i32,
    face: String,
}

/// A device context: where it draws, what is selected into it, and the text state
/// a string is drawn with.
struct Dc {
    /// The DirectDraw surface this is the DC of, when it is one.
    surface: Option<u32>,
    /// Whether this is the window's context, which draws on whatever primary
    /// surface the guest has at the moment it draws rather than on one fixed
    /// surface.
    window: bool,
    /// The bitmap selected into it, which a blit reads from.
    bitmap: Option<u32>,
    /// The font selected into it.
    font: Option<u32>,
    /// The brush `PatBlt` fills with.
    brush: Option<u32>,
    /// `SetTextColor`'s colour, as a `COLORREF`.
    text_color: u32,
    /// `SetBkMode`'s mode: 1 is `TRANSPARENT`, 2 is `OPAQUE`.
    background: u32,
    /// `SetBkColor`, when a guest sets one. Windows defaults it to white.
    background_color: u32,
}

impl Default for Dc {
    fn default() -> Self {
        Self {
            surface: None,
            window: false,
            bitmap: None,
            font: None,
            brush: None,
            // Windows starts a context with black text on a white opaque ground.
            text_color: 0,
            background: 2,
            background_color: 0x00ff_ffff,
        }
    }
}

/// GDI's objects and contexts.
#[derive(Default)]
pub struct Gdi {
    bitmaps: HashMap<u32, Bitmap>,
    fonts: HashMap<u32, Font>,
    /// Solid brushes, by handle, holding the `COLORREF` they were created with.
    brushes: HashMap<u32, u32>,
    dcs: HashMap<u32, Dc>,
    next_dc: u32,
    next_bitmap: u32,
    next_font: u32,
    next_brush: u32,
    /// Whether the notice that this host substitutes its own font has been said.
    /// One per session: a guest draws a string per line, and the fact does not
    /// change between them.
    font_notice: bool,
}

impl Gdi {
    /// Registers a bitmap resource: its handle is the one the guest already
    /// holds, so `GetObjectA` and a blit find it by that.
    pub fn add_bitmap(&mut self, memory: &GuestMemory, handle: u32, rva: u32) -> Result<()> {
        let header_size = memory.read_u32(rva)?;
        if header_size < BITMAPINFOHEADER {
            return Err(format!(
                "bitmap at {rva:#010x} has a {header_size}-byte header, which is not a BITMAPINFOHEADER"
            ));
        }
        let width = memory.read_u32(rva + 4)? as i32;
        let height = memory.read_u32(rva + 8)? as i32;
        let bits = memory.read_u16(rva + 14)?;
        let compression = memory.read_u32(rva + 16)?;
        if compression != BI_RGB {
            return Err(format!(
                "bitmap at {rva:#010x} is compressed ({compression}), which this host cannot read"
            ));
        }
        // A palettised DIB carries one BGRA entry per colour, right after the
        // header; a truecolour one carries none.
        let entries = if bits <= 8 { 1_usize << bits } else { 0 };
        let table = rva + header_size;
        self.bitmaps.insert(
            handle,
            Bitmap {
                width,
                height,
                bits,
                pixels: table + entries as u32 * 4,
                colours: read_colours(memory, table, entries)?,
                owned: None,
            },
        );
        Ok(())
    }

    /// Registers a `.bmp` file that `LoadImageA` read from the file table: the
    /// bytes are copied into guest memory, and the header inside them says where
    /// the colour table and the pixels start.
    pub fn add_bmp(&mut self, memory: &GuestMemory, file: u32) -> Result<u32> {
        if memory.read_u16(file)? != 0x4d42 {
            return Err("LoadImageA was handed a file that is not a bitmap".to_string());
        }
        let pixels_rva = memory.read_u32(file + 0x0a)?;
        let header = file + 0x0e;
        let header_size = memory.read_u32(header)?;
        if header_size < BITMAPINFOHEADER {
            return Err(format!(
                "bitmap at {file:#010x} has a {header_size}-byte header, which is not a BITMAPINFOHEADER"
            ));
        }
        let width = memory.read_u32(header + 4)? as i32;
        let height = memory.read_u32(header + 8)? as i32;
        let bits = memory.read_u16(header + 14)?;
        let compression = memory.read_u32(header + 16)?;
        if compression != BI_RGB {
            return Err(format!(
                "bitmap at {file:#010x} is compressed ({compression}), which this host cannot read"
            ));
        }
        let entries = if bits <= 8 { 1_usize << bits } else { 0 };
        let handle = FIRST_BITMAP + 0x1000 + self.next_bitmap * 4;
        self.next_bitmap += 1;
        self.bitmaps.insert(
            handle,
            Bitmap {
                width,
                height,
                bits,
                pixels: file + pixels_rva,
                colours: read_colours(memory, header + header_size, entries)?,
                owned: None,
            },
        );
        Ok(handle)
    }

    fn bitmap(&self, handle: u32) -> Option<&Bitmap> {
        self.bitmaps.get(&handle)
    }

    /// The DC that draws into `surface`, made on first use so a second `GetDC`
    /// answers the same handle.
    pub fn dc_for_surface(&mut self, surface: u32) -> u32 {
        if let Some((dc, _)) = self.dcs.iter().find(|(_, dc)| dc.surface == Some(surface)) {
            return *dc;
        }
        self.next_dc += 1;
        let dc = FIRST_DC_HANDLE + self.next_dc * 4;
        self.dcs.insert(
            dc,
            Dc {
                surface: Some(surface),
                ..Dc::default()
            },
        );
        dc
    }

    /// The window's device context, which draws on the screen: made on first use
    /// so `GetDC` and `GetWindowDC` answer the same handle.
    pub fn window_dc(&mut self) -> u32 {
        if let Some((dc, _)) = self.dcs.iter().find(|(_, dc)| dc.window) {
            return *dc;
        }
        self.next_dc += 1;
        let dc = FIRST_DC_HANDLE + self.next_dc * 4;
        self.dcs.insert(
            dc,
            Dc {
                window: true,
                ..Dc::default()
            },
        );
        dc
    }
}

/// `CreateFontA`: the fourteen fields of a `LOGFONT`, of which two matter here.
///
/// The height and the face name are kept -- the height decides how large the
/// glyphs are drawn, and the face is what the substitution notice names -- and
/// the rest (weight, italics, charset, quality) has no effect on a host with one
/// face.
fn create_font(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let face = call
        .arg(13)
        .ok()
        .filter(|address| *address != 0)
        .and_then(|address| call.memory.read_cstr(address).ok())
        .unwrap_or_default();
    Ok(make_font(host, call.arg(0)? as i32, face))
}

/// `CreateFontW`: the same `LOGFONT`, with the face name as UTF-16.
fn create_font_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let face = call
        .arg(13)
        .ok()
        .filter(|address| *address != 0)
        .and_then(|address| call.wstr(address).ok())
        .unwrap_or_default();
    Ok(make_font(host, call.arg(0)? as i32, face))
}

/// The font both creators register: one face, tall what the guest asked for.
fn make_font(host: &mut Host, height: i32, face: String) -> Action {
    let handle = FONT_HANDLE + host.gdi.next_font * 4;
    host.gdi.next_font += 1;
    host.gdi.fonts.insert(handle, Font { height, face });
    Return(handle)
}

fn get_stock_object(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let index = call.arg(0)?;
    if index > LAST_STOCK_INDEX {
        return Ok(Return(0));
    }
    Ok(Return(FIRST_OBJECT + index * 4))
}

/// `CreateCompatibleDC(hdc)`: a context with nothing selected into it. The
/// argument is ignored because a memory DC in this host has no format of its own
/// -- the blit reads the destination's format from the surface it lands in.
fn create_compatible_dc(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.gdi.next_dc += 1;
    let dc = FIRST_DC_HANDLE + host.gdi.next_dc * 4;
    host.gdi.dcs.insert(dc, Dc::default());
    Ok(Return(dc))
}

/// `SelectObject(dc, object)`: a bitmap becomes the one the DC reads from, a
/// font the one it writes with, and a brush the one a fill paints with.
///
/// A stock object is accepted and forgotten: a guest that selects one is
/// arranging a context, not asking a question. The previous object of the same
/// kind is the answer, as on Windows.
fn select_object(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let object = call.arg(1)?;
    let has_bitmap = host.gdi.bitmaps.contains_key(&object);
    let has_font = host.gdi.fonts.contains_key(&object);
    let has_brush = host.gdi.brushes.contains_key(&object);
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = if has_font {
        context.font.replace(object).unwrap_or(STOCK_FONT)
    } else if has_bitmap {
        context.bitmap.replace(object).unwrap_or(STOCK_BITMAP)
    } else if has_brush {
        context.brush.replace(object).unwrap_or(STOCK_BRUSH)
    } else {
        // A pen, a stock object, or something this host does not model: accepted
        // and forgotten, with a stock handle as the answer.
        1
    };
    Ok(Return(previous))
}

/// `DeleteObject`: forgets any object kind the handle names, and frees the pixels
/// GDI allocated for a DIB section.
fn delete_object(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    if let Some(bitmap) = host.gdi.bitmaps.remove(&handle) {
        if let Some(pixels) = bitmap.owned {
            host.release(pixels);
        }
        // A context holding it as its target has nothing to draw into now, which
        // is what Windows leaves a guest with after deleting a selected bitmap.
        for context in host.gdi.dcs.values_mut() {
            if context.bitmap == Some(handle) {
                context.bitmap = None;
            }
        }
    }
    host.gdi.fonts.remove(&handle);
    host.gdi.brushes.remove(&handle);
    Ok(Return(1))
}

fn delete_dc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.gdi.dcs.remove(&call.arg(0)?);
    Ok(Return(1))
}

/// `GetObjectA(handle, cb, out)`: the `BITMAP` a bitmap handle names.
///
/// `BITMAP` is 24 bytes: type, width, height, stride, planes, bits per pixel,
/// and where the bits are. A guest uses the size to create the surface the
/// bitmap will be stretched into.
fn get_object(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const BITMAP_BYTES: u32 = 24;
    let handle = call.arg(0)?;
    let bytes = call.arg(1)?;
    let destination = call.arg(2)?;
    let Some(bitmap) = host.gdi.bitmap(handle) else {
        return Ok(Return(0));
    };
    if destination == 0 || bytes < BITMAP_BYTES {
        return Ok(Return(0));
    }
    let width = bitmap.width;
    let height = bitmap.height.unsigned_abs() as i32;
    let stride = stride_of(bitmap.width, bitmap.bits) as i32;
    let mut out = Vec::with_capacity(BITMAP_BYTES as usize);
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&stride.to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&bitmap.bits.to_le_bytes());
    out.extend_from_slice(&bitmap.pixels.to_le_bytes());
    call.write(destination, &out)?;
    Ok(Return(BITMAP_BYTES))
}

/// `GetObjectW(handle, cb, out)`: the same `BITMAP`, or the `LOGFONTW` of a font.
///
/// A bitmap's structure has no characters in it, so the wide and ANSI forms are
/// the same bytes. A font's is all characters, and a guest that reads it back to
/// measure text needs the face and height it asked for -- which is what this host
/// kept when the font was created.
fn get_object_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const LOGFONT_BYTES: u32 = 92;
    let handle = call.arg(0)?;
    if host.gdi.bitmaps.contains_key(&handle) {
        return get_object(host, call);
    }
    let bytes = call.arg(1)?;
    let destination = call.arg(2)?;
    let Some(font) = host.gdi.fonts.get(&handle) else {
        return Ok(Return(0));
    };
    let (height, face) = (font.height, font.face.clone());
    if destination == 0 || bytes < LOGFONT_BYTES {
        return Ok(Return(0));
    }
    let mut out = vec![0_u8; LOGFONT_BYTES as usize];
    out[0..4].copy_from_slice(&height.to_le_bytes());
    // A normal weight, not italic, not underlined: the face is the built-in one
    // and those are the only variations this host has.
    out[16..20].copy_from_slice(&400_i32.to_le_bytes());
    for (index, unit) in face.encode_utf16().take(31).enumerate() {
        out[28 + index * 2..30 + index * 2].copy_from_slice(&unit.to_le_bytes());
    }
    call.write(destination, &out)?;
    Ok(Return(LOGFONT_BYTES))
}

/// `CreateDIBSection(hdc, info, usage, bits, section, offset)`: pixels GDI
/// allocates and the guest draws into through the pointer it gets back.
///
/// The context is ignored, because it matters on Windows only for realising a
/// palette and the palette here is the display's own; `hSection` and `offset` are
/// ignored too, since there is no mapped file to share the pixels with.
///
/// The colour table is copied out of the `BITMAPINFO`, so the guest may release
/// that structure as soon as the call returns. The pixels are a zeroed block of
/// the arena, and `DeleteObject` gives it back.
fn create_dib_section(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let info = call.arg(1)?;
    let destination = call.arg(3)?;
    if info == 0 || destination == 0 {
        // Windows needs both the description and somewhere to put the pointer.
        return Ok(Return(0));
    }
    let header_size = call.memory.read_u32(info)?;
    if header_size < BITMAPINFOHEADER {
        return Err(format!(
            "CreateDIBSection was handed a {header_size}-byte header, which is not a BITMAPINFOHEADER"
        ));
    }
    let width = call.memory.read_u32(info + 4)? as i32;
    let height = call.memory.read_u32(info + 8)? as i32;
    let planes = call.memory.read_u16(info + 12)?;
    let bits = call.memory.read_u16(info + 14)?;
    let compression = call.memory.read_u32(info + 16)?;
    if planes != 1 {
        return Err(format!(
            "CreateDIBSection was handed a bitmap with {planes} planes"
        ));
    }
    if compression != BI_RGB {
        return Err(format!(
            "CreateDIBSection was handed a compressed bitmap ({compression}), which this host cannot read"
        ));
    }
    if width <= 0 || height == 0 {
        return Err(format!(
            "CreateDIBSection was handed a {width}x{height} bitmap"
        ));
    }
    if !matches!(bits, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(format!(
            "CreateDIBSection was handed a {bits}-bit bitmap: this host has 1, 4, 8, 16, 24 and 32\
             -bit ones"
        ));
    }
    // A negative height is a top-down DIB, which is what a menu surface is: the
    // sign is kept in the bitmap so its rows read the way they were written.
    let size = stride_of(width, bits)
        .checked_mul(height.unsigned_abs())
        .ok_or_else(|| {
            format!("CreateDIBSection was handed a {width}x{height} DIB that is too large")
        })?;
    let pixels = host.allocate(size, 4)?;
    // A palettised DIB carries one entry per colour, or as many as `biClrUsed`
    // says when the guest described fewer than the depth allows.
    let entries = if bits <= 8 {
        let full = 1_usize << bits;
        let used = call.memory.read_u32(info + 32)? as usize;
        if used == 0 {
            full
        } else {
            used.min(full)
        }
    } else {
        0
    };
    let colours = read_colours(call.memory, info + header_size, entries)?;
    let handle = FIRST_BITMAP + 0x1000 + host.gdi.next_bitmap * 4;
    host.gdi.next_bitmap += 1;
    host.gdi.bitmaps.insert(
        handle,
        Bitmap {
            width,
            height,
            bits,
            pixels,
            colours,
            owned: Some(pixels),
        },
    );
    call.write_u32(destination, pixels)?;
    Ok(Return(handle))
}

/// `CreateSolidBrush(colour)`: a brush a fill paints a rectangle with.
fn create_solid_brush(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let colour = call.arg(0)?;
    let handle = FIRST_BRUSH + host.gdi.next_brush * 4;
    host.gdi.next_brush += 1;
    host.gdi.brushes.insert(handle, colour);
    Ok(Return(handle))
}

/// Where a device context's pixels are: a DirectDraw surface, or the bitmap the
/// guest selected into the context.
///
/// A blit and a fill work on one of these on each side, so a DIB section can be
/// the destination of a drawing *and* the source of what lands on the screen --
/// which is what a menu built off screen and then copied up does.
pub(crate) struct Device {
    /// Guest address of the first byte of the rows that are stored first.
    pub(crate) buffer: u32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) pitch: u32,
    pub(crate) bits: u8,
    /// Whether row 0 in memory is the bottom row of the picture: a DIB with a
    /// positive height stores its rows that way, a surface does not.
    bottom_up: bool,
    /// The colour table a palettised device indexes with.
    colours: Vec<[u8; 3]>,
    /// The surface this is, when it is one: drawing into it is a frame the page
    /// shows.
    pub(crate) surface: Option<u32>,
}

impl Device {
    /// Whether a picture coordinate is inside the device, which is where every
    /// read and write clips.
    fn holds(&self, x: i32, y: i32) -> bool {
        x >= 0 && y >= 0 && x < self.width && y < self.height
    }

    /// The row in memory that holds picture row `y`.
    pub(crate) fn row(&self, y: i32) -> u32 {
        if self.bottom_up {
            (self.height - 1 - y) as u32
        } else {
            y as u32
        }
    }

    /// A device with no pixels: what the window's context draws into while the
    /// guest has no display of its own. Every read misses and every write is
    /// dropped, so a call through it succeeds and shows nothing -- which is what
    /// drawing to a window nobody can see does.
    fn nowhere() -> Self {
        Self {
            buffer: 0,
            width: 0,
            height: 0,
            pitch: 0,
            bits: 8,
            bottom_up: false,
            colours: Vec::new(),
            surface: None,
        }
    }
}

/// Gives the window's context a screen, when the guest draws with GDI before it
/// has a DirectDraw display.
///
/// The screen is the size of the guest's window and as deep as the port's display,
/// so a menu built in a DIB and blitted through the window's context lands where a
/// page can read it. It is made once, on the first `GetDC`, and given up when the
/// guest creates a primary surface of its own.
pub(crate) fn ensure_window_screen(host: &mut Host) -> Result<()> {
    if host.ddraw.primary_surface().is_some() || host.ddraw.window_surface().is_some() {
        return Ok(());
    }
    // The guest's own window says how large the screen is: a game whose menus are
    // 794x550 does not have a 640x480 display.
    let (width, height) = match host
        .windows
        .size(host.windows.main_window())
        .filter(|(width, height)| *width > 0 && *height > 0)
    {
        Some(size) => size,
        None => host.windows.screen(),
    };
    let (width, height) = (width as u32, height as u32);
    if width == 0 || height == 0 {
        return Ok(());
    }
    // Truecolour, whatever depth the port's DirectDraw screen is: GDI has no
    // palette of its own, so a palettised screen would have no entries to map the
    // guest's colours through and every pixel would come out black.
    let bits = 32_u8;
    let pitch = width * 4;
    let size = pitch * height;
    let buffer = host.allocate(size, 4)?;
    host.ddraw
        .make_window_surface(buffer, width, height, pitch, size, bits);
    Ok(())
}

/// The device a context draws into: its surface, else the bitmap selected into
/// it.
///
/// The window's context is the exception: it belongs to the screen rather than to
/// one surface, so it reads whichever primary surface the guest has at the moment
/// it draws, not the one it had when it asked for the context.
pub(crate) fn device_of(host: &Host, dc: u32) -> Result<Device> {
    let Some(context) = host.gdi.dcs.get(&dc) else {
        return Err(format!("DC {dc:#010x} does not exist"));
    };
    if context.window {
        // The window's context draws on the screen: the guest's own display when it
        // has made one, otherwise the screen the host made for it.
        return match host
            .ddraw
            .primary_surface()
            .or_else(|| host.ddraw.window_surface())
        {
            Some(surface) => surface_device(host, dc, surface),
            None => Ok(Device::nowhere()),
        };
    }
    if let Some(surface) = context.surface {
        return surface_device(host, dc, surface);
    }
    let Some(handle) = context.bitmap else {
        return Err(format!("DC {dc:#010x} has no bitmap selected into it"));
    };
    let Some(bitmap) = host.gdi.bitmap(handle) else {
        return Err(format!(
            "DC {dc:#010x} has bitmap {handle:#010x} selected, which no longer exists"
        ));
    };
    Ok(Device {
        buffer: bitmap.pixels,
        width: bitmap.width,
        height: bitmap.height.unsigned_abs() as i32,
        pitch: stride_of(bitmap.width, bitmap.bits),
        bits: bitmap.bits as u8,
        bottom_up: bitmap.height > 0,
        colours: bitmap.colours.clone(),
        surface: None,
    })
}

/// The device of the DirectDraw surface a context draws on.
fn surface_device(host: &Host, dc: u32, surface: u32) -> Result<Device> {
    let Some((buffer, width, height, pitch, bits)) = host.ddraw.surface(surface) else {
        return Err(format!(
            "DC {dc:#010x} draws into surface {surface:#010x}, which does not exist"
        ));
    };
    Ok(Device {
        buffer,
        width: width as i32,
        height: height as i32,
        pitch,
        bits,
        bottom_up: false,
        colours: host.ddraw.palette().to_vec(),
        surface: Some(surface),
    })
}

/// One pixel of a device, as an RGB triple, or `None` outside it.
pub(crate) fn read_pixel(
    memory: &GuestMemory,
    device: &Device,
    x: i32,
    y: i32,
) -> Result<Option<(u8, u8, u8)>> {
    if !device.holds(x, y) {
        return Ok(None);
    }
    let row = device.buffer + device.pitch * device.row(y);
    let column = x as u32;
    let colour = match device.bits {
        1 | 4 | 8 => {
            // A palettised row packs its pixels into bytes, most significant bits
            // first: eight one-bit pixels, two four-bit ones, or one.
            let (at, shift, mask) = match device.bits {
                8 => (row + column, 0, 0xff),
                4 => (row + column / 2, if column % 2 == 0 { 4 } else { 0 }, 0x0f),
                _ => (row + column / 8, 7 - column % 8, 0x01),
            };
            let index = usize::from((memory.read_u8(at)? >> shift) & mask);
            device
                .colours
                .get(index)
                .map_or((0, 0, 0), |[red, green, blue]| (*red, *green, *blue))
        }
        16 => unpack_565(memory.read_u16(row + column * 2)?),
        24 => {
            let at = row + column * 3;
            (
                memory.read_u8(at + 2)?,
                memory.read_u8(at + 1)?,
                memory.read_u8(at)?,
            )
        }
        _ => {
            // A 32-bit device stores `0x00RRGGBB` in little-endian bytes -- blue
            // first -- which is also what a page reads it as.
            let value = memory.read_u32(row + column * 4)?;
            ((value >> 16) as u8, (value >> 8) as u8, value as u8)
        }
    };
    Ok(Some(colour))
}

/// The alpha of one pixel of a device, which only a 32-bit device stores.
///
/// `use_alpha` is a blend's `AC_SRC_ALPHA`: without it the source is opaque, as
/// GDI treats a bitmap that has no alpha channel, and a device that stores fewer
/// than thirty-two bits has none to give.
pub(crate) fn read_alpha(
    memory: &GuestMemory,
    device: &Device,
    x: i32,
    y: i32,
    use_alpha: bool,
) -> Result<u8> {
    if !use_alpha || device.bits != 32 || !device.holds(x, y) {
        return Ok(255);
    }
    let row = device.buffer + device.pitch * device.row(y);
    let value = memory.read_u32(row + x as u32 * 4)?;
    Ok((value >> 24) as u8)
}

/// A device being drawn into, with its colour conversion memoised.
///
/// An 8-bit device only accepts its own table's entries, so a colour has to be
/// matched to the nearest one. Doing that per pixel made a full-screen copy
/// search the palette 640x480 times; the memo searches it once per distinct
/// colour, and palettised artwork has no more of those than the table holds.
pub(crate) struct Painter {
    pub(crate) device: Device,
    /// The destination value each colour already resolved to, keyed by the
    /// colour's packed `0x00RRGGBB`.
    matched: HashMap<u32, u8>,
}

impl Painter {
    pub(crate) fn new(device: Device) -> Self {
        Self {
            device,
            matched: HashMap::new(),
        }
    }

    fn holds(&self, x: i32, y: i32) -> bool {
        self.device.holds(x, y)
    }

    /// The surface this paints, when it is one: drawing into it is a frame the
    /// page shows.
    pub(crate) fn surface(&self) -> Option<u32> {
        self.device.surface
    }

    /// Writes one pixel with the source's alpha, for a copy between 32-bit
    /// devices.
    ///
    /// `SRCCOPY` on two 32-bit bitmaps moves a pixel's four bytes as they stand,
    /// so the alpha channel of a DIB section survives the copy. A game that loads
    /// its artwork as ARGB and converts it through such a copy loses the whole
    /// channel if this host writes the colour alone -- every pixel then arrives
    /// transparent, and the menu it drew becomes invisible.
    pub(crate) fn put_with_alpha(
        &mut self,
        memory: &mut GuestMemory,
        x: i32,
        y: i32,
        colour: (u8, u8, u8),
        alpha: u8,
    ) -> Result<()> {
        if self.device.bits == 32 {
            let at = self.device.buffer
                + self.device.pitch * self.device.row(y)
                + x as u32 * 4;
            if !self.device.holds(x, y) {
                return Ok(());
            }
            return memory.write_u32(
                at,
                u32::from(alpha) << 24
                    | u32::from(colour.0) << 16
                    | u32::from(colour.1) << 8
                    | u32::from(colour.2),
            );
        }
        self.put(memory, x, y, colour)
    }

    /// Writes one pixel, converting the colour to what the device stores.
    pub(crate) fn put(
        &mut self,
        memory: &mut GuestMemory,
        x: i32,
        y: i32,
        colour: (u8, u8, u8),
    ) -> Result<()> {
        let Self { device, matched } = self;
        if !device.holds(x, y) {
            return Ok(());
        }
        let at =
            device.buffer + device.pitch * device.row(y) + x as u32 * (u32::from(device.bits) / 8);
        match device.bits {
            1 | 4 => Err(format!(
                "writing into a {}-bit device: this host writes 8, 16, 24 and 32 bits",
                device.bits
            )),
            8 => {
                let key = u32::from(colour.0) << 16 | u32::from(colour.1) << 8 | u32::from(colour.2);
                let index = match matched.get(&key) {
                    Some(index) => *index,
                    None => {
                        let index = nearest(&device.colours, colour);
                        matched.insert(key, index);
                        index
                    }
                };
                memory.write_u8(at, index)
            }
            16 => memory.write_u16(at, pack_565(colour.0, colour.1, colour.2)),
            24 => {
                memory.write_u8(at, colour.2)?;
                memory.write_u8(at + 1, colour.1)?;
                memory.write_u8(at + 2, colour.0)
            }
            _ => memory.write_u32(
                at,
                u32::from(colour.0) << 16 | u32::from(colour.1) << 8 | u32::from(colour.2),
            ),
        }
    }
}

impl Device {
    /// The guest address of one pixel of a 32-bit device, or `None` outside it.
    ///
    /// A row copy needs the address arithmetic directly: going through the
    /// per-pixel reader costs a scale match and a bounds check a pixel, and a
    /// frame is half a million pixels.
    pub(crate) fn address32(&self, x: i32, y: i32) -> Option<u32> {
        if self.bits != 32 || !self.holds(x, y) {
            return None;
        }
        Some(self.buffer + self.pitch * self.row(y) + x as u32 * 4)
    }
}

/// Copies a rectangle between two 32-bit devices, a row a `memmove`.
///
/// The per-pixel path answers every case -- scaling, a palettised side, a
/// rectangle that hangs off an edge -- but a frame is presented by copying a
/// whole 32-bit screen onto another, and doing that a pixel at a time was most of
/// what a guest's frame cost. Returns whether it ran; anything it does not cover
/// falls back to the loop rather than being approximated.
pub(crate) fn copy_rows_32(
    memory: &mut GuestMemory,
    source: &Device,
    dest: &Device,
    (source_x, source_y): (i32, i32),
    (dest_x, dest_y): (i32, i32),
    width: i32,
    height: i32,
) -> Result<bool> {
    if source.bits != 32 || dest.bits != 32 || width <= 0 || height <= 0 {
        return Ok(false);
    }
    // Overlapping rectangles of one device would need a direction-aware move, and
    // no target of this kit blits a surface onto itself.
    if source.buffer == dest.buffer {
        return Ok(false);
    }
    // The whole rectangle has to be inside both devices: a blit that hangs off an
    // edge copies the part that exists, and that clipping belongs to the loop.
    if !source.holds(source_x, source_y)
        || !source.holds(source_x + width - 1, source_y + height - 1)
        || !dest.holds(dest_x, dest_y)
        || !dest.holds(dest_x + width - 1, dest_y + height - 1)
    {
        return Ok(false);
    }
    let bytes = width as usize * 4;
    for row in 0..height {
        let from = source.buffer + source.pitch * source.row(source_y + row) + source_x as u32 * 4;
        let to = dest.buffer + dest.pitch * dest.row(dest_y + row) + dest_x as u32 * 4;
        memory.copy(from, to, bytes)?;
    }
    Ok(true)
}

/// The colour a blit's raster operation fills with, for the two operations that
/// ignore the source; `None` for one that reads it.
fn fill_colour(rop: u32) -> Option<(u8, u8, u8)> {
    match rop {
        BLACKNESS => Some((0, 0, 0)),
        WHITENESS => Some((255, 255, 255)),
        _ => None,
    }
}

/// Paints a rectangle one colour, clipped to the device.
fn fill_rect(
    call: &mut Call<'_>,
    painter: &mut Painter,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    colour: (u8, u8, u8),
) -> Result<()> {
    for row in 0..height.max(0) {
        for column in 0..width.max(0) {
            painter.put(call.memory, x + column, y + row, colour)?;
        }
    }
    Ok(())
}

/// `BitBlt(dest, x, y, w, h, source, sx, sy, rop)`: copies a rectangle between
/// two contexts, one pixel for one.
///
/// Both sides are devices, so the copy runs in either direction: a DIB section
/// onto the screen, or the screen captured back into one. `BLACKNESS` and
/// `WHITENESS` ignore the source and fill the rectangle, which is how a guest
/// clears a window it has not painted yet.
fn bit_blt(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dest_dc = call.arg(0)?;
    let (dest_x, dest_y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let (width, height) = (call.arg(3)? as i32, call.arg(4)? as i32);
    let source_dc = call.arg(5)?;
    let (source_x, source_y) = (call.arg(6)? as i32, call.arg(7)? as i32);
    let rop = call.arg(8)?;
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
        host.notice(format!(
            "BitBlt dest {dest_dc:#x} window {} ({dest_x},{dest_y} {width}x{height}) \
             source {source_dc:#x} window {} ({source_x},{source_y}) rop {rop:#x}",
            host.gdi.dcs.get(&dest_dc).map_or(false, |context| context.window),
            host.gdi.dcs.get(&source_dc).map_or(false, |context| context.window),
        ));
    }
    // A context that is not there is a call Windows answers FALSE, so it is
    // answered rather than stopping the machine: a guest clearing a window it has
    // no usable context for carries on.
    let Ok(dest) = device_of(host, dest_dc) else {
        return Ok(Return(0));
    };
    let mut dest = Painter::new(dest);
    if let Some(colour) = fill_colour(rop) {
        // These read no source at all, so a null one is not an error.
        fill_rect(call, &mut dest, dest_x, dest_y, width, height, colour)?;
        if let Some(surface) = dest.surface() {
            host.ddraw.touch(surface);
        }
        return Ok(Return(1));
    }
    if rop != SRCCOPY {
        return Err(format!(
            "BitBlt with raster op {rop:#010x}: this host copies (SRCCOPY) and fills (BLACKNESS, \
             WHITENESS) and no other op"
        ));
    }
    let Ok(source) = device_of(host, source_dc) else {
        return Ok(Return(0));
    };
    if width <= 0 || height <= 0 {
        return Ok(Return(1));
    }
    // A copy between two 32-bit devices moves the alpha byte with the colour, so
    // a row move is exactly what the loop below computes.
    if copy_rows_32(
        call.memory,
        &source,
        &dest.device,
        (source_x, source_y),
        (dest_x, dest_y),
        width,
        height,
    )? {
        if let Some(surface) = dest.surface() {
            host.ddraw.touch(surface);
        }
        return Ok(Return(1));
    }
    let alpha_source = source.bits == 32;
    for row in 0..height {
        for column in 0..width {
            // A source pixel outside the device is left alone rather than
            // painted, which is what clipping a blit at the edge does.
            let Some(colour) = read_pixel(call.memory, &source, source_x + column, source_y + row)?
            else {
                continue;
            };
            let alpha = if alpha_source {
                read_alpha(call.memory, &source, source_x + column, source_y + row, true)?
            } else {
                0
            };
            dest.put_with_alpha(call.memory, dest_x + column, dest_y + row, colour, alpha)?;
        }
    }
    if let Some(surface) = dest.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(1))
}

/// `PatBlt(dc, x, y, w, h, rop)`: fills a rectangle of the context.
///
/// `PATCOPY` paints the selected brush, which is the solid colour
/// `CreateSolidBrush` was given; a context with none selected paints white, the
/// stock brush Windows starts one with.
fn pat_blt(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let (width, height) = (call.arg(3)? as i32, call.arg(4)? as i32);
    let rop = call.arg(5)?;
    let invert = match rop {
        PATCOPY | WHITENESS | BLACKNESS => false,
        DSTINVERT => true,
        _ => {
            return Err(format!(
                "PatBlt with raster op {rop:#010x}: this host fills (PATCOPY, WHITENESS, BLACKNESS, \
                 DSTINVERT) and no other op"
            ))
        }
    };
    let colour = match rop {
        WHITENESS => (255, 255, 255),
        BLACKNESS => (0, 0, 0),
        _ => colour_of(
            host.gdi
                .dcs
                .get(&dc)
                .and_then(|context| context.brush)
                .and_then(|handle| host.gdi.brushes.get(&handle).copied())
                .unwrap_or(0x00ff_ffff),
        ),
    };
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(0));
    };
    let mut painter = Painter::new(device);
    if !invert {
        fill_rect(call, &mut painter, x, y, width, height, colour)?;
    } else {
        for row in 0..height.max(0) {
            for column in 0..width.max(0) {
                let Some(current) = read_pixel(call.memory, &painter.device, x + column, y + row)?
                else {
                    continue;
                };
                painter.put(
                    call.memory,
                    x + column,
                    y + row,
                    (255 - current.0, 255 - current.1, 255 - current.2),
                )?;
            }
        }
    }
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(1))
}

/// `StretchBlt(dest, x, y, w, h, source, sx, sy, sw, sh, rop)`.
fn stretch_blt(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dest_dc = call.arg(0)?;
    let (dest_x, dest_y, dest_w, dest_h) = (
        call.arg(1)? as i32,
        call.arg(2)? as i32,
        call.arg(3)? as i32,
        call.arg(4)? as i32,
    );
    let source_dc = call.arg(5)?;
    let (source_x, source_y, source_w, source_h) = (
        call.arg(6)? as i32,
        call.arg(7)? as i32,
        call.arg(8)? as i32,
        call.arg(9)? as i32,
    );
    let rop = call.arg(10)?;
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
        host.notice(format!(
            "StretchBlt dest {dest_dc:#x} ({dest_x},{dest_y} {dest_w}x{dest_h}) \
             source {source_dc:#x} ({source_x},{source_y} {source_w}x{source_h}) rop {rop:#x}"
        ));
    }
    let Ok(dest) = device_of(host, dest_dc) else {
        return Ok(Return(0));
    };
    let mut dest = Painter::new(dest);
    if let Some(colour) = fill_colour(rop) {
        fill_rect(call, &mut dest, dest_x, dest_y, dest_w, dest_h, colour)?;
        if let Some(surface) = dest.surface() {
            host.ddraw.touch(surface);
        }
        return Ok(Return(1));
    }
    if rop != SRCCOPY {
        return Err(format!(
            "StretchBlt with raster op {rop:#010x}: this host copies (SRCCOPY) and fills (BLACKNESS, \
             WHITENESS) and no other op"
        ));
    }
    let Ok(source) = device_of(host, source_dc) else {
        if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
            host.notice(format!("StretchBlt source {source_dc:#x} has no device"));
        }
        return Ok(Return(0));
    };
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI")
        && host.gdi.dcs.get(&dest_dc).map_or(false, |context| context.window)
    {
        host.notice(format!(
            "StretchBlt to window dest {dest_dc:#x} at {dest_x},{dest_y} {dest_w}x{dest_h} \
             from {source_dc:#x} {source_w}x{source_h}"
        ));
    }
    if dest_w <= 0 || dest_h <= 0 || source_w <= 0 || source_h <= 0 {
        return Ok(Return(1));
    }
    // At one for one, a copy between two 32-bit devices is a row move -- and a
    // menu that is composed on a surface and then put up is exactly that.
    if source_w == dest_w
        && source_h == dest_h
        && copy_rows_32(
            call.memory,
            &source,
            &dest.device,
            (source_x, source_y),
            (dest_x, dest_y),
            source_w,
            source_h,
        )?
    {
        if let Some(surface) = dest.surface() {
            host.ddraw.touch(surface);
        }
        return Ok(Return(1));
    }
    // Both rectangles are in picture coordinates: `read_pixel` has already
    // accounted for a bottom-up DIB, so the scaling arithmetic does not care
    // which way a device stores its rows.
    let alpha_source = source.bits == 32;
    for row in 0..dest_h {
        let source_row = source_y + (row as i64 * i64::from(source_h) / i64::from(dest_h)) as i32;
        for column in 0..dest_w {
            let source_column =
                source_x + (column as i64 * i64::from(source_w) / i64::from(dest_w)) as i32;
            let Some(colour) = read_pixel(call.memory, &source, source_column, source_row)? else {
                continue;
            };
            let alpha = if alpha_source {
                read_alpha(call.memory, &source, source_column, source_row, true)?
            } else {
                0
            };
            dest.put_with_alpha(call.memory, dest_x + column, dest_y + row, colour, alpha)?;
        }
    }
    if let Some(surface) = dest.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(1))
}

/// The 16-bit form of a colour: five bits of red, six of green, five of blue.
fn pack_565(red: u8, green: u8, blue: u8) -> u16 {
    (u16::from(red >> 3) << 11) | (u16::from(green >> 2) << 5) | u16::from(blue >> 3)
}

/// The colour a 16-bit pixel stores, read back to eight bits a channel.
fn unpack_565(value: u16) -> (u8, u8, u8) {
    (
        ((value >> 11) & 0x1f) as u8 * 255 / 31,
        ((value >> 5) & 0x3f) as u8 * 255 / 63,
        (value & 0x1f) as u8 * 255 / 31,
    )
}

/// The palette entry closest to `colour`, by squared distance, so a blit or a
/// pixel write between two palettised devices keeps the artwork's colours.
fn nearest(palette: &[[u8; 3]], colour: (u8, u8, u8)) -> u8 {
    let mut best = 0_usize;
    let mut best_distance = i32::MAX;
    for (index, entry) in palette.iter().enumerate() {
        let dr = i32::from(entry[0]) - i32::from(colour.0);
        let dg = i32::from(entry[1]) - i32::from(colour.1);
        let db = i32::from(entry[2]) - i32::from(colour.2);
        let distance = dr * dr + dg * dg + db * db;
        if distance < best_distance {
            best_distance = distance;
            best = index;
            if distance == 0 {
                break;
            }
        }
    }
    best as u8
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};
    use crate::thunks;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    fn call_shim(
        host: &mut Host,
        memory: &mut GuestMemory,
        arguments: &[u32],
    ) -> Result<Action> {
        let args = STACK_BASE + 0x4000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: 4,
        };
        get_stock_object(host, &mut call)
    }

    #[test]
    fn a_font_reads_back_as_the_logfontw_it_was_created_from() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.gdi.fonts.insert(
            FONT_HANDLE,
            Font {
                height: -20,
                face: "Courier New".to_string(),
            },
        );
        let args = STACK_BASE + 0x4000;
        let buffer = STACK_BASE + 0x4100;
        let mut bytes = Vec::new();
        for argument in [FONT_HANDLE, 92, buffer] {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).unwrap();
        memory.write_bytes(buffer, &[0xaa; 92]).unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(get_object_w(&mut host, &mut call).unwrap(), Return(92));
        // `lfHeight` first, and `lfFaceName` as UTF-16 at offset 28.
        assert_eq!(call.memory.read_u32(buffer).unwrap() as i32, -20);
        let face: Vec<u8> = "Courier New"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(call.memory.read_bytes(buffer + 28, face.len()).unwrap(), face);

        // A handle that is neither a bitmap nor a font is not an object here.
        host.gdi.fonts.remove(&FONT_HANDLE);
        assert_eq!(
            get_object_w(
                &mut host,
                &mut Call {
                    memory: &mut memory,
                    args,
                    arg_bytes: 12,
                }
            )
            .unwrap(),
            Return(0),
            "a deleted font answers nothing"
        );
    }

    #[test]
    fn every_stock_object_gets_its_own_non_null_handle() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let white = call_shim(&mut host, &mut memory, &[0]).unwrap();
        let black = call_shim(&mut host, &mut memory, &[4]).unwrap();
        assert_eq!(white, Return(FIRST_OBJECT));
        assert_eq!(black, Return(FIRST_OBJECT + 16));
        assert_ne!(white, black);
    }

    #[test]
    fn an_index_that_is_not_a_stock_object_is_null() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, &[LAST_STOCK_INDEX + 1]).unwrap(),
            Return(0)
        );
    }

    #[test]
    fn a_bitmap_resource_reports_its_size_and_its_bits() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A 4-bit 16x2 bitmap: a 40-byte header, sixteen palette entries and the
        // pixels, laid out the way `LoadImageA` hands them over.
        let at = STACK_BASE + 0x8000;
        let mut dib = Vec::new();
        dib.extend_from_slice(&40_u32.to_le_bytes());
        dib.extend_from_slice(&16_i32.to_le_bytes());
        dib.extend_from_slice(&2_i32.to_le_bytes());
        dib.extend_from_slice(&1_u16.to_le_bytes());
        dib.extend_from_slice(&4_u16.to_le_bytes());
        dib.extend_from_slice(&0_u32.to_le_bytes());
        dib.extend_from_slice(&16_u32.to_le_bytes());
        dib.resize(40 + 64, 0);
        memory.write_bytes(at, &dib).unwrap();

        let handle = FIRST_DC_HANDLE + 0x400;
        host.gdi.add_bitmap(&memory, handle, at).unwrap();
        let out = STACK_BASE + 0x9000;
        let args = STACK_BASE + 0x4000;
        memory
            .write_bytes(
                args,
                &[handle, 24, out]
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect::<Vec<u8>>(),
            )
            .unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(get_object(&mut host, &mut call).unwrap(), Return(24));
        let mut words = [0_u32; 6];
        for (index, word) in words.iter_mut().enumerate() {
            *word = call.memory.read_u32(out + index as u32 * 4).unwrap();
        }
        assert_eq!(words[0], 0);
        assert_eq!(words[1], 16);
        assert_eq!(words[2], 2);
        // 16 pixels of four bits is a stride of 8, which is already a multiple
        // of four.
        assert_eq!(words[3], 8);
        assert_eq!(words[4] & 0xffff, 1);
        assert_eq!(words[4] >> 16, 4);
        assert_eq!(words[5], at + 40 + 64);
    }

    #[test]
    fn a_colour_takes_the_nearest_entry_the_destination_can_show() {
        let mut palette = [[0_u8; 3]; 256];
        palette[10] = [255, 0, 0];
        palette[20] = [0, 255, 0];
        palette[30] = [0, 0, 255];
        assert_eq!(nearest(&palette, (255, 0, 0)), 10);
        assert_eq!(nearest(&palette, (0, 255, 0)), 20);
        assert_eq!(nearest(&palette, (0, 0, 255)), 30);
        // A colour the palette does not have lands on the closest one it does.
        assert_eq!(nearest(&palette, (250, 4, 4)), 10);
    }

    /// Runs one of this module's bindings by name.
    fn dispatch(
        host: &mut Host,
        memory: &mut GuestMemory,
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("{name} is implemented"));
        let args = STACK_BASE + 0x4c00;
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

    /// A palettised primary surface made the way a guest makes one, so text has
    /// something to land in. No entries are supplied: DirectDraw still reserves
    /// index 0 black and index 255 white, which is the ink these tests draw in.
    fn surface(host: &mut Host, memory: &mut GuestMemory, width: u32, height: u32) -> u32 {
        let palette_args = STACK_BASE + 0x4f00;
        let palette_out = STACK_BASE + 0x5080;
        write_args(memory, palette_args, &[0, 0, 0, palette_out]);
        let mut call = Call {
            memory,
            args: palette_args,
            arg_bytes: 16,
        };
        crate::shim::ddraw::serve(host, &mut call, thunks::DDRAW, "CreatePalette")
            .unwrap();

        // A `DDSURFACEDESC`: size, flags, height, width, pitch, and the
        // capabilities word near its end.
        let desc = STACK_BASE + 0x5100;
        let destination = STACK_BASE + 0x5280;
        memory.write_u32(desc, 108).unwrap();
        memory.write_u32(desc + 4, 0x1 | 0x2 | 0x4).unwrap();
        memory.write_u32(desc + 8, height).unwrap();
        memory.write_u32(desc + 12, width).unwrap();
        memory.write_u32(desc + 16, 0).unwrap();
        memory.write_u32(desc + 104, 0x200).unwrap();
        let args = STACK_BASE + 0x5300;
        write_args(memory, args, &[0, desc, destination, 0]);
        let mut call = Call {
            memory,
            args,
            arg_bytes: 16,
        };
        crate::shim::ddraw::serve(host, &mut call, thunks::DDRAW, "CreateSurface")
            .unwrap();
        call.memory.read_u32(destination).unwrap()
    }

    /// `IDirectDrawSurface::GetDC` through the shim: the route a guest takes to
    /// a device context over a surface, and the one Cave Story draws text with.
    fn dc_for(host: &mut Host, memory: &mut GuestMemory, surface: u32) -> u32 {
        let out = STACK_BASE + 0x5320;
        let args = STACK_BASE + 0x5300;
        write_args(memory, args, &[surface, out]);
        let mut call = Call {
            memory,
            args,
            arg_bytes: 8,
        };
        assert_eq!(
            crate::shim::ddraw::serve(host, &mut call, thunks::DDRAW_SURFACE, "GetDC").unwrap(),
            Return(0)
        );
        call.memory.read_u32(out).unwrap()
    }

    fn write_args(memory: &mut GuestMemory, at: u32, arguments: &[u32]) {
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(at, &bytes).unwrap();
    }

    /// `CreateFontA(lfHeight, .., lfFaceName)` through the shim: the two fields
    /// of a `LOGFONT` this host reads.
    fn font(host: &mut Host, memory: &mut GuestMemory, height: i32, face: &str) -> u32 {
        let name = STACK_BASE + 0x5400;
        memory.write_bytes(name, face.as_bytes()).unwrap();
        memory.write_u8(name + face.len() as u32, 0).unwrap();
        let mut arguments = [0_u32; 14];
        arguments[0] = height as u32;
        arguments[13] = name;
        let args = STACK_BASE + 0x5480;
        write_args(memory, args, &arguments);
        let mut call = Call {
            memory,
            args,
            arg_bytes: 56,
        };
        let Return(handle) = create_font(host, &mut call).unwrap() else {
            panic!("CreateFontA answers with a handle");
        };
        handle
    }

    /// `TextOutA(dc, x, y, string, count)` through the shim.
    fn draw_text(
        host: &mut Host,
        memory: &mut GuestMemory,
        dc: u32,
        x: i32,
        y: i32,
        string: u32,
        count: i32,
    ) -> Result<Action> {
        let args = STACK_BASE + 0x5500;
        write_args(
            memory,
            args,
            &[dc, x as u32, y as u32, string, count as u32],
        );
        let mut call = Call {
            memory,
            args,
            arg_bytes: 20,
        };
        text_out(host, &mut call)
    }

    /// The palette index stored at a pixel of the 8-bit surface.
    fn index_at(host: &Host, memory: &GuestMemory, surface: u32, x: u32, y: u32) -> u8 {
        let (buffer, _, _, pitch, bits) = host.ddraw.surface(surface).expect("the surface");
        assert_eq!(bits, 8);
        memory.read_u8(buffer + pitch * y + x).unwrap()
    }

    #[test]
    fn text_out_draws_the_glyphs_of_the_selected_font() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = surface(&mut host, &mut memory, 64, 32);
        let dc = dc_for(&mut host, &mut memory, surface);
        let font = font(&mut host, &mut memory, 20, "Courier New");
        assert_eq!(
            dispatch(&mut host, &mut memory, "SelectObject", &[dc, font]).unwrap(),
            Return(STOCK_FONT),
            "a fresh context starts on the stock font, so that is what it answers"
        );
        // A guest starts a context on an opaque ground, and the credits set it
        // transparent: the artwork behind the line shows through.
        assert_eq!(
            dispatch(&mut host, &mut memory, "SetBkMode", &[dc, 1]).unwrap(),
            Return(2)
        );
        assert_eq!(
            dispatch(&mut host, &mut memory, "SetTextColor", &[dc, 0x00ff_ffff]).unwrap(),
            Return(0)
        );

        let string = STACK_BASE + 0x5600;
        memory.write_bytes(string, b"HA").unwrap();
        assert_eq!(
            draw_text(&mut host, &mut memory, dc, 4, 4, string, 2).unwrap(),
            Return(1)
        );

        // Twenty pixels is the face's own size, so the glyphs are drawn as
        // designed: two pixels of left bearing, then `H`'s two stems with the gap
        // between them unlit.
        assert_eq!(index_at(&host, &memory, surface, 6, 7), 255);
        assert_eq!(index_at(&host, &memory, surface, 7, 7), 0);
        assert_eq!(index_at(&host, &memory, surface, 13, 7), 255);
        // The three rows above the cap height are the face's own blank band.
        assert_eq!(index_at(&host, &memory, surface, 6, 6), 0);
        // The crossbar sits in the middle of the cap height, across all eight
        // columns.
        assert_eq!(index_at(&host, &memory, surface, 9, 13), 255);
        // The next glyph starts twelve pixels along, and `A`'s first inked row is
        // its two middle columns.
        assert_eq!(index_at(&host, &memory, surface, 18, 7), 0);
        assert_eq!(index_at(&host, &memory, surface, 21, 7), 255);
        // The row below the face's descenders is the surface's own pixel.
        assert_eq!(index_at(&host, &memory, surface, 6, 24), 0);
        // Text into the surface the page shows is a frame like any other.
        assert!(host.ddraw.content_revision() > 0);
        // The substitution is the player's business, so it is said out loud.
        assert!(
            host.notices.iter().any(|notice| notice.contains("TextOutA")),
            "{:?}",
            host.notices
        );
    }

    #[test]
    fn an_opaque_background_fills_the_cell_behind_the_glyphs() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = surface(&mut host, &mut memory, 64, 32);
        let dc = dc_for(&mut host, &mut memory, surface);
        let font = font(&mut host, &mut memory, 20, "Courier New");
        dispatch(&mut host, &mut memory, "SelectObject", &[dc, font]).unwrap();
        dispatch(&mut host, &mut memory, "SetBkMode", &[dc, 2]).unwrap();
        dispatch(&mut host, &mut memory, "SetTextColor", &[dc, 0]).unwrap();

        let string = STACK_BASE + 0x5600;
        memory.write_bytes(string, b"H").unwrap();
        draw_text(&mut host, &mut memory, dc, 2, 2, string, 1).unwrap();

        // Black ink on the white ground Windows starts a context with.
        assert_eq!(index_at(&host, &memory, surface, 4, 5), 0);
        assert_eq!(index_at(&host, &memory, surface, 5, 5), 255);
        // The cell is twelve columns by twenty rows: the last column and the rows
        // past the twentieth are ground, and what lies beyond them is the
        // surface's own pixel.
        assert_eq!(index_at(&host, &memory, surface, 13, 5), 255);
        assert_eq!(index_at(&host, &memory, surface, 2, 21), 255);
        assert_eq!(index_at(&host, &memory, surface, 14, 5), 0);
        assert_eq!(index_at(&host, &memory, surface, 2, 22), 0);
    }

    #[test]
    fn a_taller_font_draws_larger_glyphs() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = surface(&mut host, &mut memory, 64, 64);
        let dc = dc_for(&mut host, &mut memory, surface);
        // Forty pixels is two cells, so the face is drawn twice as large.
        let font = font(&mut host, &mut memory, 40, "Arial");
        dispatch(&mut host, &mut memory, "SelectObject", &[dc, font]).unwrap();
        dispatch(&mut host, &mut memory, "SetBkMode", &[dc, 1]).unwrap();
        dispatch(&mut host, &mut memory, "SetTextColor", &[dc, 0x00ff_ffff]).unwrap();

        let string = STACK_BASE + 0x5600;
        memory.write_bytes(string, b"H").unwrap();
        draw_text(&mut host, &mut memory, dc, 4, 4, string, 1).unwrap();

        // Twice the size: the left bearing four pixels wide, the stem two pixels
        // wide, and each row of the face two rows further down.
        assert_eq!(index_at(&host, &memory, surface, 8, 10), 255);
        assert_eq!(index_at(&host, &memory, surface, 9, 10), 255);
        assert_eq!(index_at(&host, &memory, surface, 10, 10), 0);
        assert_eq!(index_at(&host, &memory, surface, 8, 11), 255);
        // The crossbar is the ninth row of the face, so twenty-two pixels down.
        assert_eq!(index_at(&host, &memory, surface, 14, 22), 255);
    }

    #[test]
    fn a_count_below_zero_reads_to_the_end_of_the_string() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = surface(&mut host, &mut memory, 64, 32);
        let dc = dc_for(&mut host, &mut memory, surface);
        let font = font(&mut host, &mut memory, 20, "Courier New");
        dispatch(&mut host, &mut memory, "SelectObject", &[dc, font]).unwrap();
        dispatch(&mut host, &mut memory, "SetTextColor", &[dc, 0x00ff_ffff]).unwrap();

        let string = STACK_BASE + 0x5600;
        memory.write_bytes(string, b"H\0A").unwrap();
        draw_text(&mut host, &mut memory, dc, 4, 4, string, -1).unwrap();

        // The `A` past the terminator is not drawn, so its cell stays blank.
        assert_eq!(index_at(&host, &memory, surface, 6, 7), 255);
        assert_eq!(index_at(&host, &memory, surface, 21, 7), 0);
    }

    #[test]
    fn text_into_a_context_that_does_not_exist_names_the_call() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let string = STACK_BASE + 0x5600;
        memory.write_bytes(string, b"x").unwrap();
        let error = draw_text(&mut host, &mut memory, 0xdead_beef, 0, 0, string, 1).unwrap_err();
        assert!(error.contains("TextOutA"), "{error}");
    }

    /// `CreateDIBSection` for a `width` x `rows` bitmap: answers the handle and
    /// the pixel pointer the guest was given. The height is signed the way a
    /// DIB's is, so a negative one is a top-down bitmap.
    fn dib(
        host: &mut Host,
        memory: &mut GuestMemory,
        width: i32,
        height: i32,
        bits: u16,
        palette: &[[u8; 3]],
    ) -> (u32, u32) {
        let info = STACK_BASE + 0x6000;
        let out = STACK_BASE + 0x6100;
        let mut header = Vec::new();
        header.extend_from_slice(&40_u32.to_le_bytes());
        header.extend_from_slice(&width.to_le_bytes());
        header.extend_from_slice(&height.to_le_bytes());
        header.extend_from_slice(&1_u16.to_le_bytes());
        header.extend_from_slice(&bits.to_le_bytes());
        header.resize(40, 0);
        memory.write_bytes(info, &header).unwrap();
        for (index, [red, green, blue]) in palette.iter().enumerate() {
            memory
                .write_bytes(info + 40 + index as u32 * 4, &[*blue, *green, *red, 0])
                .unwrap();
        }
        let args = STACK_BASE + 0x6200;
        write_args(memory, args, &[0, info, 0, out, 0, 0]);
        let action = {
            let mut call = Call {
                memory,
                args,
                arg_bytes: 24,
            };
            create_dib_section(host, &mut call).expect("CreateDIBSection answers")
        };
        let Return(handle) = action else {
            panic!("CreateDIBSection answers with a handle");
        };
        (handle, memory.read_u32(out).unwrap())
    }

    /// A memory context with `object` selected into it, which is the route a
    /// guest takes to a DIB it created.
    fn context(host: &mut Host, memory: &mut GuestMemory, object: u32) -> u32 {
        let dc = dispatch(host, memory, "CreateCompatibleDC", &[0]).unwrap();
        let Return(dc) = dc else {
            panic!("CreateCompatibleDC answers with a handle");
        };
        assert_ne!(
            dispatch(host, memory, "SelectObject", &[dc, object]).unwrap(),
            Return(0),
            "a fresh context answers the stock object it starts with, not null"
        );
        dc
    }

    /// `USER32!GetDC`: the context the window draws through.
    fn window_dc(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        let binding = crate::shim::user32::SHIMS
            .iter()
            .find(|binding| binding.name == "GetDC")
            .expect("GetDC is implemented");
        let args = STACK_BASE + 0x6700;
        write_args(memory, args, &[0]);
        let mut call = Call {
            memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        let Return(dc) = (binding.shim)(host, &mut call).unwrap() else {
            panic!("GetDC answers a handle");
        };
        dc
    }

    #[test]
    fn a_dib_section_hands_the_guest_pixels_it_can_draw_into() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The 32-bit top-down DIB a menu surface is: 3x2 pixels.
        let (handle, pixels) = dib(&mut host, &mut memory, 3, -2, 32, &[]);
        assert_ne!(handle, 0, "CreateDIBSection answers a handle");
        assert_ne!(pixels, 0, "and a pixel pointer");

        // The guest owns those pixels: a write through the pointer is a write
        // GDI reads. `0x00RRGGBB` is the WORD a 32-bit DIB stores.
        memory.write_u32(pixels + 4, 0x00ff_0000).unwrap();
        let dc = context(&mut host, &mut memory, handle);
        assert_eq!(
            dispatch(&mut host, &mut memory, "GetPixel", &[dc, 1, 0]).unwrap(),
            Return(0x0000_00ff),
            "the red pixel the guest wrote reads back as a COLORREF"
        );

        // `GetObjectA` reports the size GDI will blit from: three columns, two
        // rows, a twelve-byte stride, and the pixels the guest holds.
        let out = STACK_BASE + 0x6300;
        let args = STACK_BASE + 0x6200;
        write_args(&mut memory, args, &[handle, 24, out]);
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(get_object(&mut host, &mut call).unwrap(), Return(24));
        // `BITMAP`: the type word, then the size, the stride and the bits.
        assert_eq!(call.memory.read_u32(out).unwrap(), 0, "bmType is zero");
        assert_eq!(call.memory.read_u32(out + 4).unwrap(), 3, "bmWidth");
        assert_eq!(call.memory.read_u32(out + 8).unwrap(), 2, "bmHeight");
        assert_eq!(call.memory.read_u32(out + 12).unwrap(), 12, "bmWidthBytes");
        assert_eq!(call.memory.read_u32(out + 20).unwrap(), pixels, "bmBits");

        // Deleting the bitmap returns its pixels to the arena and leaves the
        // context it was selected into with no target.
        assert!(host.allocated(pixels).is_some());
        assert_eq!(
            dispatch(&mut host, &mut memory, "DeleteObject", &[handle]).unwrap(),
            Return(1)
        );
        assert!(host.allocated(pixels).is_none(), "the pixels are released");
        assert_eq!(
            dispatch(&mut host, &mut memory, "GetPixel", &[dc, 1, 0]).unwrap(),
            Return(CLR_INVALID)
        );
    }

    #[test]
    fn a_bottom_up_dib_is_read_from_its_last_row() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A positive height is the bottom-up DIB a resource carries: the first
        // row in memory is the bottom row of the picture.
        let (handle, pixels) = dib(&mut host, &mut memory, 1, 2, 32, &[]);
        memory.write_u32(pixels, 0x00ff_ffff).unwrap();
        memory.write_u32(pixels + 4, 0x00ff_0000).unwrap();
        let dc = context(&mut host, &mut memory, handle);
        assert_eq!(
            dispatch(&mut host, &mut memory, "GetPixel", &[dc, 0, 0]).unwrap(),
            Return(0x0000_00ff),
            "picture row 0 is the second row in memory"
        );
        assert_eq!(
            dispatch(&mut host, &mut memory, "GetPixel", &[dc, 0, 1]).unwrap(),
            Return(0x00ff_ffff)
        );
    }

    #[test]
    fn a_bit_blt_copies_one_dib_into_another() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let (source, source_pixels) = dib(&mut host, &mut memory, 3, -1, 32, &[]);
        let (destination, destination_pixels) = dib(&mut host, &mut memory, 3, -1, 32, &[]);
        for (index, colour) in [0x00ff_0000_u32, 0x0000_ff00, 0x0000_00ff].iter().enumerate() {
            memory
                .write_u32(source_pixels + index as u32 * 4, *colour)
                .unwrap();
        }
        let source_dc = context(&mut host, &mut memory, source);
        let destination_dc = context(&mut host, &mut memory, destination);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[destination_dc, 0, 0, 3, 1, source_dc, 0, 0, SRCCOPY]
            )
            .unwrap(),
            Return(1)
        );
        for index in 0..3 {
            let at = destination_pixels + index * 4;
            assert_eq!(
                memory.read_u32(at).unwrap(),
                memory.read_u32(source_pixels + index * 4).unwrap(),
                "pixel {index} arrives unchanged"
            );
        }

        // The one raster operation this host copies with is named, so a guest
        // asking for another is told which.
        let error = dispatch(
            &mut host,
            &mut memory,
            "BitBlt",
            &[destination_dc, 0, 0, 1, 1, source_dc, 0, 0, 0x0033_0000],
        )
        .unwrap_err();
        assert!(error.contains("SRCCOPY"), "{error}");
    }

    #[test]
    fn a_bit_blt_reaches_the_surface_through_its_palette() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = surface(&mut host, &mut memory, 8, 8);
        let destination = dc_for(&mut host, &mut memory, surface);
        let (handle, pixels) = dib(&mut host, &mut memory, 2, -1, 32, &[]);
        memory.write_u32(pixels, 0x00ff_ffff).unwrap();
        memory.write_u32(pixels + 4, 0).unwrap();
        let source = context(&mut host, &mut memory, handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[destination, 1, 1, 2, 1, source, 0, 0, SRCCOPY]
            )
            .unwrap(),
            Return(1)
        );
        // DirectDraw's reserved pair: index 0 black, index 255 white.
        assert_eq!(index_at(&host, &memory, surface, 1, 1), 255);
        assert_eq!(index_at(&host, &memory, surface, 2, 1), 0);
        assert_eq!(index_at(&host, &memory, surface, 0, 1), 0, "nothing left of the blit");
        // Putting a menu on the surface the page shows is a frame.
        assert!(host.ddraw.content_revision() > 0);
    }

    #[test]
    fn a_pat_blt_fills_with_the_selected_brush() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let (handle, pixels) = dib(&mut host, &mut memory, 4, -2, 32, &[]);
        let dc = context(&mut host, &mut memory, handle);
        let Return(brush) = dispatch(&mut host, &mut memory, "CreateSolidBrush", &[0x0000_00ff])
            .unwrap()
        else {
            panic!("CreateSolidBrush answers with a handle");
        };
        assert_eq!(
            dispatch(&mut host, &mut memory, "SelectObject", &[dc, brush]).unwrap(),
            Return(STOCK_BRUSH),
            "the previous brush is the stock white one"
        );

        // The second row, two columns wide, in red.
        assert_eq!(
            dispatch(&mut host, &mut memory, "PatBlt", &[dc, 0, 1, 2, 1, PATCOPY]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(pixels + 16).unwrap(), 0x00ff_0000);
        assert_eq!(memory.read_u32(pixels + 20).unwrap(), 0x00ff_0000);
        assert_eq!(memory.read_u32(pixels + 24).unwrap(), 0, "the fill stops at two columns");

        // `WHITENESS` paints white whatever is selected, and `DSTINVERT` turns
        // the red it painted back to its complement.
        dispatch(&mut host, &mut memory, "PatBlt", &[dc, 3, 0, 1, 1, WHITENESS]).unwrap();
        assert_eq!(memory.read_u32(pixels + 12).unwrap(), 0x00ff_ffff);
        dispatch(&mut host, &mut memory, "PatBlt", &[dc, 0, 1, 1, 1, DSTINVERT]).unwrap();
        assert_eq!(memory.read_u32(pixels + 16).unwrap(), 0x0000_ffff);
    }

    #[test]
    fn the_window_context_draws_on_the_primary_surface() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_dc(&mut host, &mut memory);
        // Before the guest has a display there is nowhere to put the pixels: the
        // call succeeds and nothing is shown, as it does on a window nobody can
        // see.
        assert_eq!(
            dispatch(&mut host, &mut memory, "BitBlt", &[window, 0, 0, 4, 4, 0, 0, 0, BLACKNESS])
                .unwrap(),
            Return(1)
        );

        // Once there is one, the same context draws on it: a white blit lands on
        // the surface the page reads, and it is a frame.
        let surface = surface(&mut host, &mut memory, 8, 8);
        assert_eq!(
            dispatch(&mut host, &mut memory, "BitBlt", &[window, 0, 0, 4, 4, 0, 0, 0, WHITENESS])
                .unwrap(),
            Return(1)
        );
        assert_eq!(index_at(&host, &memory, surface, 3, 3), 255);
        assert_eq!(index_at(&host, &memory, surface, 4, 0), 0, "the fill is 4x4");
        assert!(host.ddraw.content_revision() > 0);
    }

    #[test]
    fn a_menu_built_off_screen_lands_through_the_window_context() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = surface(&mut host, &mut memory, 8, 8);
        // The menu is assembled in a top-down 32-bit DIB -- what a game creates
        // with `CreateDIBSection` and draws into through the pointer it was
        // given -- and then copied up with one `BitBlt` through the window's own
        // context, which is the whole route a menu takes to the page.
        let (handle, pixels) = dib(&mut host, &mut memory, 2, -2, 32, &[]);
        for index in 0..4 {
            memory.write_u32(pixels + index * 4, 0x00ff_ffff).unwrap();
        }
        let menu = context(&mut host, &mut memory, handle);
        let window = window_dc(&mut host, &mut memory);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[window, 3, 2, 2, 2, menu, 0, 0, SRCCOPY]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(index_at(&host, &memory, surface, 3, 2), 255);
        assert_eq!(index_at(&host, &memory, surface, 4, 3), 255);
        assert_eq!(
            index_at(&host, &memory, surface, 2, 2),
            0,
            "nothing left of the 2x2 menu"
        );
        assert!(host.ddraw.content_revision() > 0, "the page has a frame to read");
    }

    #[test]
    fn a_gdi_menu_has_a_screen_before_the_guest_has_a_display() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A program whose menus are drawn with GDI never creates a DirectDraw
        // surface, and its window still has to show something.
        assert!(host.ddraw.primary_surface().is_none());
        let window = window_dc(&mut host, &mut memory);
        let screen = host
            .ddraw
            .window_surface()
            .expect("GetDC gave the window a screen");
        assert_eq!(host.ddraw.bits(), 32, "GDI has no palette to map through");

        // A pixel blitted through the window's context is a pixel a page reads.
        let (handle, pixels) = dib(&mut host, &mut memory, 1, -1, 32, &[]);
        memory.write_u32(pixels, 0x0012_3456).unwrap();
        let menu = context(&mut host, &mut memory, handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[window, 7, 9, 1, 1, menu, 0, 0, SRCCOPY]
            )
            .unwrap(),
            Return(1)
        );
        let (buffer, _, _) = host.ddraw.framebuffer().expect("the guest has a screen");
        let (_, _, _, pitch, _) = host.ddraw.surface(screen).expect("the screen is a surface");
        assert_eq!(
            memory.read_u32(buffer + pitch * 9 + 7 * 4).unwrap(),
            0x0012_3456,
            "the menu's pixel is where the page reads it"
        );
        assert!(
            host.ddraw.content_revision() > 0,
            "the page has a frame to read"
        );
    }

    #[test]
    fn the_windows_screen_is_given_up_when_the_guest_makes_a_display() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        window_dc(&mut host, &mut memory);
        assert!(host.ddraw.window_surface().is_some());
        let surface = surface(&mut host, &mut memory, 8, 8);
        assert!(
            host.ddraw.window_surface().is_none(),
            "the guest's own primary surface is the screen now"
        );
        assert_eq!(
            host.ddraw.framebuffer().map(|(buffer, _, _)| buffer),
            host.ddraw.primary_buffer(),
            "and the page reads the guest's display"
        );
        assert_eq!(host.ddraw.primary_surface(), Some(surface));
    }

    #[test]
    fn a_blit_through_a_context_that_is_not_there_answers_false() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // Windows fails these calls rather than faulting, and a guest that clears
        // a window before it has a context carries on either way.
        for name in ["BitBlt", "StretchBlt", "PatBlt"] {
            let arguments: &[u32] = match name {
                "BitBlt" => &[0, 0, 0, 1, 1, 0, 0, 0, BLACKNESS],
                "StretchBlt" => &[0, 0, 0, 1, 1, 0, 0, 0, 1, 1, BLACKNESS],
                _ => &[0, 0, 0, 1, 1, BLACKNESS],
            };
            assert_eq!(
                dispatch(&mut host, &mut memory, name, arguments).unwrap(),
                Return(0),
                "{name} through no context"
            );
        }
    }

    #[test]
    fn text_out_draws_into_a_dib_section() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A menu's own surface: text laid out off screen and blitted up later.
        let (handle, pixels) = dib(&mut host, &mut memory, 32, -24, 32, &[]);
        let dc = context(&mut host, &mut memory, handle);
        let font = font(&mut host, &mut memory, 20, "Courier New");
        dispatch(&mut host, &mut memory, "SelectObject", &[dc, font]).unwrap();
        dispatch(&mut host, &mut memory, "SetBkMode", &[dc, 1]).unwrap();
        dispatch(&mut host, &mut memory, "SetTextColor", &[dc, 0x00ff_ffff]).unwrap();

        let string = STACK_BASE + 0x6800;
        memory.write_bytes(string, b"H").unwrap();
        assert_eq!(
            draw_text(&mut host, &mut memory, dc, 4, 4, string, 1).unwrap(),
            Return(1)
        );
        // The same cell the surface path draws: two pixels of bearing, the stem
        // six pixels in, at `0x00RRGGBB` per pixel.
        let read = |x: u32, y: u32| memory.read_u32(pixels + (y * 32 + x) * 4).unwrap();
        assert_eq!(read(6, 7), 0x00ff_ffff);
        assert_eq!(read(7, 7), 0);
        assert_eq!(read(13, 7), 0x00ff_ffff);
        // Text into a DIB is not a frame: the page shows nothing yet.
        assert_eq!(host.ddraw.content_revision(), 0);
    }

    #[test]
    fn create_font_w_reads_the_wide_face() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let face = STACK_BASE + 0x6400;
        let units: Vec<u16> = "\u{7d30}\u{660e}\u{9ad4}"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        for (index, unit) in units.iter().enumerate() {
            memory.write_u16(face + index as u32 * 2, *unit).unwrap();
        }
        let mut arguments = [0_u32; 14];
        arguments[0] = (-20_i32) as u32;
        arguments[13] = face;
        let args = STACK_BASE + 0x6500;
        write_args(&mut memory, args, &arguments);
        let action = {
            let mut call = Call {
                memory: &mut memory,
                args,
                arg_bytes: 56,
            };
            create_font_w(&mut host, &mut call).unwrap()
        };
        let Return(handle) = action else {
            panic!("CreateFontW answers with a handle");
        };

        // `GetObjectW` gives the face back as the UTF-16 it was created from,
        // which is what a guest reads to measure its own text.
        let out = STACK_BASE + 0x6600;
        write_args(&mut memory, args, &[handle, 92, out]);
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(get_object_w(&mut host, &mut call).unwrap(), Return(92));
        assert_eq!(call.memory.read_u32(out).unwrap() as i32, -20);
        let expected: Vec<u8> = units
            .iter()
            .flat_map(|unit| unit.to_le_bytes())
            .collect();
        assert_eq!(
            call.memory.read_bytes(out + 28, expected.len()).unwrap(),
            expected
        );
    }
}
