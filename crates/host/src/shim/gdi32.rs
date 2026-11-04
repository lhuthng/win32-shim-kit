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
//! decides how large the glyphs are drawn. The faces are the Bitstream Vera
//! family embedded in the module (`assets/fonts/`), rasterised at whatever
//! height the guest's `LOGFONT` asks for: "Courier New" resolves to Vera Mono,
//! "Arial" to Vera, "Times New Roman" to Vera Serif, and a heavy weight picks
//! the bold twin. That is a substitution a player can see, so it is said once
//! per session, and a character the face does not have is drawn as a box rather
//! than dropped.

use ab_glyph::{Font as _, ScaleFont as _};
use std::collections::HashMap;

use crate::memory::GuestMemory;
use crate::shim::{Action, Binding, Call, Host};

/// Whether the GDI trace is on. A blit asks this on every call, and a guest draws
/// its frame out of thousands of them, so it is read once.
static LOGGING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
/// `SHIMKIT_LOG_GDI_SURFACE` traces the scaled copies onto DirectDraw surfaces.
static SURFACE_LOGGING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
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

/// Palette handles, in the object space above the brushes so a log line can say
/// which kind of object a number is.
const FIRST_PALETTE: u32 = FIRST_OBJECT + 0x400;

/// A palette entry index no palette has written, so a palette that was created
/// and never filled realizes nothing instead of clearing the display to black.
const NO_INDEX: usize = usize::MAX;

/// `CreateDIBitmap`'s `CBM_INIT`: the caller supplies the pixels.
const CBM_INIT: u32 = 4;

/// The only `LOGPALETTE` version Windows has ever defined.
const PALETTE_VERSION: u16 = 0x0300;

/// `ETO_CLIPPED`: the call carries a rectangle its text must not leave.
const ETO_CLIPPED: u32 = 4;

/// `GetClipBox`'s region complexities, and the error code: a guest that asks
/// whether anything is visible reads them.
const REGION_ERROR: u32 = 0;
const COMPLEXREGION: u32 = 1;
const SIMPLEREGION: u32 = 2;
const NULLREGION: u32 = 3;

/// The `GetDeviceCaps` indices this host answers. The rest answer zero, which is
/// what a device without that capability reports.
const HORZRES: u32 = 8;
const VERTRES: u32 = 10;
const BITSPIXEL: u32 = 12;
const PLANES: u32 = 14;
const NUMCOLORS: u32 = 24;
const RASTERCAPS: u32 = 38;
const LOGPIXELSX: u32 = 88;
const LOGPIXELSY: u32 = 90;
const SIZEPALETTE: u32 = 104;

/// `RC_BITBLT`: the one raster capability this device has, since every blit it
/// is asked for is a copy through `Painter`.
const RC_BITBLT: u32 = 1;

/// The dots per inch this host reports. A browser canvas has no physical size, so
/// the conventional 96 keeps a guest that sizes a dialog in inches consistent.
const DOTS_PER_INCH: u32 = 96;

/// `SRCCOPY`: the only raster operation a blit is asked for here.
const SRCCOPY: u32 = 0x00cc_0020;

/// The raster operations `PatBlt` fills with: the selected brush, all white, all
/// black, and the inverse of what is already there.
const PATCOPY: u32 = 0x00f0_0021;
const WHITENESS: u32 = 0x00ff_0062;
const BLACKNESS: u32 = 0x0000_0042;
const DSTINVERT: u32 = 0x0055_0009;
/// The ops a game composes sprites with: `SRCAND` blits a mask, which keeps the
/// sprite's shape and clears everything else, then `SRCPAINT` blits the image
/// through the cleared shape. A host that refuses them draws neither pass, and
/// the character reads as holes punched through the background.
const SRCAND: u32 = 0x0088_00c6;
const SRCPAINT: u32 = 0x00ee_0086;
const SRCINVERT: u32 = 0x0066_0046;
const NOTSRCCOPY: u32 = 0x0033_0008;
const SRCERASE: u32 = 0x0044_0328;
/// Every op that combines a source pixel with a destination one and needs no
/// brush. `BLACKNESS` and `WHITENESS` fill and never read a source, so they are
/// answered before this list is consulted.
const ROP_MASKING: [u32; 6] = [SRCCOPY, SRCAND, SRCPAINT, SRCINVERT, NOTSRCCOPY, SRCERASE];

/// The built-in face's cell: twelve pixels of advance, of which two are the left
/// bearing, eight are the glyph's own columns and two are letter spacing. Twenty
/// rows are cap height plus descenders, the size Courier New would be drawn at
/// when a guest asks for a twenty-pixel font.
const GLYPH_HEIGHT: i32 = 20;

// ---- the embedded faces ----------------------------------------------------

/// The faces embedded in the module, which `TextOutA` rasterises instead of the
/// hand-drawn table this shim used to carry. Bitstream Vera, taken from Wine's
/// font share: `VeraMono` is the standard "Courier New" substitute, `Vera` the
/// sans, `VeraSe` the serif — each with a bold twin for heavy weights.
mod embedded {
    pub const SANS: &[u8] = include_bytes!("../../assets/fonts/Vera.ttf");
    pub const SANS_BOLD: &[u8] = include_bytes!("../../assets/fonts/VeraBd.ttf");
    pub const MONO: &[u8] = include_bytes!("../../assets/fonts/VeraMono.ttf");
    pub const MONO_BOLD: &[u8] = include_bytes!("../../assets/fonts/VeraMoBd.ttf");
    pub const SERIF: &[u8] = include_bytes!("../../assets/fonts/VeraSe.ttf");
    pub const SERIF_BOLD: &[u8] = include_bytes!("../../assets/fonts/VeraSeBd.ttf");
}

/// Which embedded face a `LOGFONT` resolves to. The name picks the family, the
/// weight picks the twin; `u8` so a raster cache can key on it directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Face(pub u8);

impl Face {
    pub const SANS: u8 = 0;
    pub const SANS_BOLD: u8 = 1;
    pub const MONO: u8 = 2;
    pub const MONO_BOLD: u8 = 3;
    pub const SERIF: u8 = 4;
    pub const SERIF_BOLD: u8 = 5;

    /// Resolves a `LOGFONT`'s face name and weight. Wine's own substitution
    /// table maps "Courier New" to Vera Mono, "Arial" to Vera and "Times New
    /// Roman" to Vera Serif, and this follows it.
    pub fn of(face: &str, weight: i32) -> Self {
        let bold = weight >= 700;
        let name = face.to_ascii_lowercase();
        let family = if name.contains("courier")
            || name.contains("mono")
            || name.contains("consol")
        {
            "mono"
        } else if name.contains("times")
            || name.contains("serif")
            || name.contains("georgia")
            || name.contains("roman")
        {
            "serif"
        } else {
            "sans"
        };
        let base = match (family, bold) {
            ("mono", false) => Self::MONO,
            ("mono", true) => Self::MONO_BOLD,
            ("serif", false) => Self::SERIF,
            ("serif", true) => Self::SERIF_BOLD,
            (_, false) => Self::SANS,
            (_, true) => Self::SANS_BOLD,
        };
        Self(base)
    }
}

/// One rasterised glyph: coverage per pixel, the metrics to place it, and the
/// advance the next character starts at.
struct RasterGlyph {
    /// Pixels between the draw x and the glyph's first column.
    left: i32,
    /// Pixels between the draw y (the cell's top) and the glyph's first row.
    top: i32,
    width: u32,
    height: u32,
    /// 0-255 per pixel, row-major.
    coverage: Vec<u8>,
    advance: i32,
}

/// The six embedded faces, parsed once per session.
struct Faces {
    faces: [ab_glyph::FontVec; 6],
}

impl Faces {
    fn embedded() -> Self {
        let parse = |bytes: &'static [u8]| {
            ab_glyph::FontVec::try_from_vec(bytes.to_vec())
                .expect("the embedded font parses")
        };
        Self {
            faces: [
                parse(embedded::SANS),
                parse(embedded::SANS_BOLD),
                parse(embedded::MONO),
                parse(embedded::MONO_BOLD),
                parse(embedded::SERIF),
                parse(embedded::SERIF_BOLD),
            ],
        }
    }

    fn get(&self, variant: u8) -> &ab_glyph::FontVec {
        &self.faces[variant as usize % self.faces.len()]
    }
}

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
    Binding { dll: GDI32, name: "CreatePalette", arg_bytes: 4, shim: create_palette },
    Binding { dll: GDI32, name: "SetPaletteEntries", arg_bytes: 16, shim: set_palette_entries },
    Binding { dll: GDI32, name: "SelectPalette", arg_bytes: 12, shim: select_palette },
    Binding { dll: GDI32, name: "RealizePalette", arg_bytes: 4, shim: realize_palette },
    Binding { dll: GDI32, name: "UnrealizeObject", arg_bytes: 4, shim: unrealize_object },
    Binding { dll: GDI32, name: "GetDeviceCaps", arg_bytes: 8, shim: get_device_caps },
    Binding { dll: GDI32, name: "GetTextExtentPointA", arg_bytes: 16, shim: get_text_extent_point },
    Binding { dll: GDI32, name: "ExtTextOutA", arg_bytes: 32, shim: ext_text_out },
    Binding { dll: GDI32, name: "GetClipBox", arg_bytes: 8, shim: get_clip_box },
    Binding { dll: GDI32, name: "IntersectClipRect", arg_bytes: 20, shim: intersect_clip_rect },
    Binding { dll: GDI32, name: "PtVisible", arg_bytes: 12, shim: pt_visible },
    Binding { dll: GDI32, name: "RectVisible", arg_bytes: 8, shim: rect_visible },
    Binding { dll: GDI32, name: "SaveDC", arg_bytes: 4, shim: save_dc },
    Binding { dll: GDI32, name: "RestoreDC", arg_bytes: 8, shim: restore_dc },
    Binding { dll: GDI32, name: "SetMapMode", arg_bytes: 8, shim: set_map_mode },
    Binding { dll: GDI32, name: "SetWindowExtEx", arg_bytes: 16, shim: set_window_ext },
    Binding { dll: GDI32, name: "SetViewportExtEx", arg_bytes: 16, shim: set_viewport_ext },
    Binding { dll: GDI32, name: "SetViewportOrgEx", arg_bytes: 16, shim: set_viewport_org },
    Binding { dll: GDI32, name: "OffsetViewportOrgEx", arg_bytes: 16, shim: offset_viewport_org },
    Binding { dll: GDI32, name: "ScaleWindowExtEx", arg_bytes: 24, shim: scale_window_ext },
    Binding { dll: GDI32, name: "ScaleViewportExtEx", arg_bytes: 24, shim: scale_viewport_ext },
    Binding { dll: GDI32, name: "CreateBitmap", arg_bytes: 20, shim: create_bitmap },
    Binding { dll: GDI32, name: "CreateDIBitmap", arg_bytes: 24, shim: create_di_bitmap },
    Binding { dll: GDI32, name: "StretchDIBits", arg_bytes: 52, shim: stretch_dibits },
    Binding { dll: GDI32, name: "Escape", arg_bytes: 20, shim: escape },
    Binding { dll: GDI32, name: "GdiFlush", arg_bytes: 0, shim: gdi_flush },
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
        // TEMP DIAGNOSTIC: dump raw ANSI bytes that are not plain ASCII, so a
        // mis-decoded code page names itself. Capped so a frame loop cannot flood.
        if bytes.iter().any(|byte| *byte >= 0x80) {
            use std::sync::atomic::{AtomicU32, Ordering};
            static DUMPS: AtomicU32 = AtomicU32::new(0);
            if DUMPS.load(Ordering::Relaxed) < 40 && host.notices.len() < 600 {
                DUMPS.fetch_add(1, Ordering::Relaxed);
                let hex: Vec<String> =
                    bytes.iter().map(|byte| format!("{byte:02x}")).collect();
                host.notice(format!(
                    "TextOutA non-ascii bytes: {}",
                    hex.join(" ")
                ));
            }
        }
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

    if !host.gdi.dcs.contains_key(&dc) {
        return Err(format!("{method} into DC {dc:#010x}, which does not exist"));
    }
    name_the_font(host, dc, method);
    draw_run(host, call.memory, dc, x, y, &text)?;
    Ok(Return(1))
}

/// Says once per session which face GDI text is drawn with, naming the one the
/// guest asked for: a substitution is something a player can see, so it is said
/// once rather than per line.
fn name_the_font(host: &mut Host, dc: u32, method: &str) {
    if host.gdi.font_notice {
        return;
    }
    host.gdi.font_notice = true;
    let height = font_height(host, dc);
    let face = host
        .gdi
        .dcs
        .get(&dc)
        .and_then(|context| context.font)
        .and_then(|font| host.gdi.fonts.get(&font))
        .map_or_else(|| "the current font".to_string(), |font| format!("\"{}\"", font.face));
    host.notice(format!(
        "{method}: this host has no system fonts, so GDI text is drawn with its embedded \
         Bitstream Vera face at {height} pixels (the guest selected {face})"
    ));
}

/// The cell height of the font selected into a context, in pixels.
///
/// A guest asks for a height in a `LOGFONT` and then lays its lines out against
/// the one it gets back, so this is what `GetTextExtentPointA` reports and what a
/// run of text occupies.
pub(crate) fn font_height(host: &Host, dc: u32) -> i32 {
    host.gdi
        .dcs
        .get(&dc)
        .and_then(|context| context.font)
        .and_then(|font| host.gdi.fonts.get(&font))
        .map_or(GLYPH_HEIGHT, |font| font.height.unsigned_abs() as i32)
        .max(1)
}

/// Draws one run of text at `x, y` into a context, with the context's own font,
/// colours and background mode, and answers the advance the run took.
///
/// Every text call in this host is this loop. `TextOutA` draws one run; a game's
/// `ExtTextOutA`, `DrawTextA` and `TabbedTextOutA` break their strings into runs
/// and draw each with it. Sharing the loop is what makes the glyphs of a line --
/// and the width a guest measures off them -- the same whichever entry point drew
/// them, which matters because a guest measures with one call and draws with
/// another.
pub(crate) fn draw_run(
    host: &mut Host,
    memory: &mut GuestMemory,
    dc: u32,
    x: i32,
    y: i32,
    text: &str,
) -> Result<i32> {
    let Some(context) = host.gdi.dcs.get(&dc) else {
        return Err(format!("text into DC {dc:#010x}, which does not exist"));
    };
    let (foreground, background, opaque, variant, height) = {
        let text = context.text_color;
        let fill = context.background_color;
        let opaque = context.background == 2;
        let (variant, height) = context
            .font
            .and_then(|font| host.gdi.fonts.get(&font))
            .map_or((Face::MONO, GLYPH_HEIGHT as i32), |font| {
                (font.variant, font.height.unsigned_abs() as i32)
            });
        (colour_of(text), colour_of(fill), opaque, variant, height.max(1))
    };

    // The context's own device: a surface for a game drawing straight to the
    // screen, a bitmap for one laying its text out off screen to blit up later.
    let mut painter = Painter::new(device_of(host, dc)?);
    // Ink and background are both cut to the clip: a guest that scrolls a panel
    // sets one and draws the whole list through it, and `ExtTextOutA` may narrow
    // it further for a single call.
    let clip = clip_of(host, dc, &painter.device);

    // The raster comes from the cache, so a guest that draws a string per frame
    // pays for each glyph once. The ascent places the cell's top: a glyph's
    // outline bounds are relative to the baseline, which sits `ascent` pixels
    // below the draw y.
    let ascent = host.gdi.ascent(variant, height);
    let mut cursor = x;
    for character in text.chars() {
        let glyph = host.gdi.raster(variant, height, character);
        let (glyph_left, glyph_top) = (glyph.left, glyph.top);
        let (glyph_width, glyph_height) = (glyph.width as usize, glyph.height as usize);
        let advance = glyph.advance.max(1);
        if opaque {
            // The cell fills with the background colour; a pixel inside the
            // glyph's outline blends the two by its coverage, which the palette
            // turns into the closest entries it has.
            for row in 0..height {
                for column in 0..advance {
                    let (cell_x, cell_y) = (cursor + column, y + row);
                    if clipped(clip, cell_x, cell_y) {
                        continue;
                    }
                    painter.put(memory, cell_x, cell_y, background)?;
                }
            }
        }
        // A 32-bit device carries per-pixel alpha, so the coverage lands
        // directly and the edges antialias; a palettised one has no room for
        // partial inking, so opaque mode mixes ink into the background colour
        // and transparent mode inks at a coverage threshold.
        let antialiased = painter.device.bits == 32;
        for row in 0..glyph_height {
            for column in 0..glyph_width {
                let coverage = glyph.coverage[row * glyph_width + column];
                if coverage == 0 {
                    continue;
                }
                let ink_x = cursor + glyph_left + column as i32;
                let ink_y = y + ascent + glyph_top + row as i32;
                if clipped(clip, ink_x, ink_y) {
                    continue;
                }
                if antialiased {
                    painter.put_with_alpha(
                        memory,
                        ink_x,
                        ink_y,
                        foreground,
                        coverage,
                    )?;
                } else if opaque {
                    let mix = |ink: u8, paper: u8| {
                        ((u16::from(ink) * u16::from(coverage)
                            + u16::from(paper) * (255 - u16::from(coverage)))
                            / 255) as u8
                    };
                    painter.put(
                        memory,
                        ink_x,
                        ink_y,
                        (
                            mix(foreground.0, background.0),
                            mix(foreground.1, background.1),
                            mix(foreground.2, background.2),
                        ),
                    )?;
                } else if coverage >= 128 {
                    painter.put(memory, ink_x, ink_y, foreground)?;
                }
            }
        }
        cursor += advance;
    }
    // Text drawn into the surface the page shows is a frame like any other.
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(cursor)
}

/// The width and cell height of a run of text in a context's font.
///
/// The width is the sum of the advances the same glyphs are drawn with, so a
/// guest that measures a string and then draws it gets the line it laid out.
/// This is what `GetTextExtentPointA` reports.
pub(crate) fn text_extent(host: &mut Host, dc: u32, text: &str) -> (i32, i32) {
    let (variant, height) = host
        .gdi
        .dcs
        .get(&dc)
        .and_then(|context| context.font)
        .and_then(|font| host.gdi.fonts.get(&font))
        .map_or((Face::MONO, GLYPH_HEIGHT), |font| {
            (font.variant, font.height.unsigned_abs() as i32)
        });
    let height = height.max(1);
    let width = text
        .chars()
        .map(|character| host.gdi.raster(variant, height, character).advance.max(1))
        .sum();
    (width, height)
}

/// The rectangle a context's drawing is cut to: its clip, held inside the device.
///
/// A context with no clip draws over the whole device, which is the common case;
/// one that set a clip (`IntersectClipRect`, or `BeginPaint` on a region) is cut
/// to it, and to the device either way, because a fill outside the pixels would
/// be a write past the surface.
pub(crate) fn clip_of(host: &Host, dc: u32, device: &Device) -> [i32; 4] {
    let whole = [0, 0, device.width, device.height];
    let Some(clip) = host.gdi.dcs.get(&dc).and_then(|context| context.clip) else {
        return whole;
    };
    [
        clip[0].max(whole[0]),
        clip[1].max(whole[1]),
        clip[2].min(whole[2]),
        clip[3].min(whole[3]),
    ]
}

/// Fills a rectangle with one colour through the destination's palette, cut to
/// the context's clip.
///
/// `FillRect`, the background of a dialog's control and every `PatBlt` with a
/// solid brush are this: a run of pixels, converted to the entries the device
/// actually has. (`fill_rect`, below, is the blit path's inner loop: a rectangle
/// into a painter that already knows its device.)
pub(crate) fn paint_rect(
    host: &mut Host,
    memory: &mut GuestMemory,
    dc: u32,
    rect: [i32; 4],
    colour: u32,
) -> Result<()> {
    let device = device_of(host, dc)?;
    let clip = clip_of(host, dc, &device);
    let (left, top) = (rect[0].max(clip[0]), rect[1].max(clip[1]));
    let (right, bottom) = (rect[2].min(clip[2]), rect[3].min(clip[3]));
    if right <= left || bottom <= top {
        return Ok(());
    }
    let ink = colour_of(colour);
    let mut painter = Painter::new(device);
    for y in top..bottom {
        for x in left..right {
            painter.put(memory, x, y, ink)?;
        }
    }
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(())
}

/// Inverts the pixels of a rectangle, which is what `DrawFocusRect` is: the
/// focus ring a dialog draws around the control the keyboard is on.
pub(crate) fn invert_rect(
    host: &mut Host,
    memory: &mut GuestMemory,
    dc: u32,
    rect: [i32; 4],
) -> Result<()> {
    let device = device_of(host, dc)?;
    let clip = clip_of(host, dc, &device);
    let (left, top) = (rect[0].max(clip[0]), rect[1].max(clip[1]));
    let (right, bottom) = (rect[2].min(clip[2]), rect[3].min(clip[3]));
    if right <= left || bottom <= top {
        return Ok(());
    }
    let mut painter = Painter::new(device);
    for y in top..bottom {
        for x in left..right {
            if let Some((red, green, blue)) = read_pixel(memory, &painter.device, x, y)? {
                painter.put(memory, x, y, (255 - red, 255 - green, 255 - blue))?;
            }
        }
    }
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(())
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

fn colour_of(colour: u32) -> (u8, u8, u8) {
    (
        colour as u8,
        ((colour >> 8) & 0xff) as u8,
        ((colour >> 16) & 0xff) as u8,
    )
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

/// The colours of a `PALETTEENTRY` run: `peRed`, `peGreen`, `peBlue`, flags.
///
/// A `PALETTEENTRY` starts at red and a colour table's `RGBQUAD` starts at blue,
/// so reading a palette with `read_colours` swaps the red and blue of every
/// entry. Two writers share the one table the page decodes with -- GDI through
/// `RealizePalette` and DirectDraw through `SetEntries` -- and when they disagree
/// about the order the screen takes its colours from whichever wrote last, which
/// is a picture that loses its detail without anything being drawn wrongly.
fn read_palette_entries(memory: &GuestMemory, at: u32, count: usize) -> Result<Vec<[u8; 3]>> {
    let mut colours = Vec::with_capacity(count);
    for index in 0..count as u32 {
        let entry = at + index * 4;
        colours.push([
            memory.read_u8(entry)?,
            memory.read_u8(entry + 1)?,
            memory.read_u8(entry + 2)?,
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
    /// Which embedded face this `LOGFONT` resolves to.
    variant: u8,
}

/// A device context: where it draws, what is selected into it, and the text state
/// a string is drawn with.
///
/// Cloning one is what `SaveDC` and `RestoreDC` are: a saved context is a copy of
/// every field here, and restoring it puts the copy back.
#[derive(Clone)]
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
    /// The palette `SelectPalette` put in this context, which `RealizePalette`
    /// writes into the device it draws on.
    palette: Option<u32>,
    /// The clip rectangle, which every fill, blit and run of text is cut to. A
    /// guest that scrolls a panel sets one and draws inside it.
    clip: Option<[i32; 4]>,
    /// `SetMapMode`'s mode, and the extents and origins the `Set...Ex` calls set.
    /// Only `MM_TEXT` maps a unit to a pixel, which is the mode every game here
    /// uses; the rest are kept so a `Get` answers what was set.
    map_mode: i32,
    window_ext: (i32, i32),
    viewport_ext: (i32, i32),
    viewport_org: (i32, i32),
    /// `SaveDC`'s saved states, most recent last.
    saved: Vec<Dc>,
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
            palette: None,
            clip: None,
            // `MM_TEXT`: one device unit is one pixel.
            map_mode: 1,
            window_ext: (1, 1),
            viewport_ext: (1, 1),
            viewport_org: (0, 0),
            saved: Vec::new(),
        }
    }
}

/// The bytes a bitmap's pixels occupy: one row of `stride_of`, `height` times.
fn pixel_bytes(bitmap: &Bitmap) -> u64 {
    u64::from(stride_of(bitmap.width, bitmap.bits)) * u64::from(bitmap.height.unsigned_abs())
}

/// A `LOGPALETTE` the guest created, and the entries `SetPaletteEntries` wrote.
///
/// A game from this era fades its screen by rewriting a run of entries and then
/// calling `RealizePalette` -- no blit, no flip -- so the entries have to reach
/// the display's own table, and the count that changed is what tells the page
/// the picture moved. The lowest index written is kept because that is the run
/// realized: a guest that sets entries 16..32 must not overwrite entry 0, which
/// a palette created without `DDPCAPS_ALLOW256` reserves for black.
struct Palette {
    /// The lowest index written, or [`NO_INDEX`] when nothing was.
    first: usize,
    /// The entries from `first` on.
    entries: Vec<[u8; 3]>,
}

impl Palette {
    fn new() -> Self {
        Self {
            first: NO_INDEX,
            entries: Vec::new(),
        }
    }

    /// Writes `colours` at `start`, growing the run at either end so a guest may
    /// set a high index before a low one and both stay where it put them.
    fn write(&mut self, start: usize, colours: &[[u8; 3]]) {
        if colours.is_empty() {
            return;
        }
        if self.first == NO_INDEX || start < self.first {
            let gap = match self.first {
                NO_INDEX => start,
                first => first - start,
            };
            let mut grown = vec![[0_u8; 3]; gap];
            grown.extend_from_slice(&self.entries);
            self.entries = grown;
            self.first = start;
        }
        let offset = start - self.first;
        let wanted = offset + colours.len();
        if self.entries.len() < wanted {
            self.entries.resize(wanted, [0; 3]);
        }
        self.entries[offset..wanted].copy_from_slice(colours);
    }
}

/// GDI's objects and contexts.
#[derive(Default)]
pub struct Gdi {
    bitmaps: HashMap<u32, Bitmap>,
    fonts: HashMap<u32, Font>,
    /// The palettes the guest created, by handle.
    palettes: HashMap<u32, Palette>,
    next_palette: u32,
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
    /// The six embedded faces, parsed on first use.
    faces_once: std::sync::OnceLock<Faces>,
    /// Rasterised glyphs, by face, pixel height and character. A guest draws a
    /// string per line and the credits draw hundreds, so each glyph is built
    /// once and blitted many times.
    glyph_cache: HashMap<(u8, i32, char), std::rc::Rc<RasterGlyph>>,
    /// Pixel bytes this host has handed to `CreateDIBSection` and taken back
    /// through `DeleteObject`. A guest that holds what it loads shows the first
    /// total growing while the second barely moves, which is the difference
    /// between a game that needs the memory and a host that loses it.
    pixel_bytes_created: u64,
    pixel_bytes_released: u64,
}


impl Gdi {
    /// The embedded faces, parsed on first use.
    fn embedded_faces(&self) -> &Faces {
        self.faces_once.get_or_init(Faces::embedded)
    }

    /// The pixels between a string's draw y and its glyphs' baselines.
    fn ascent(&self, variant: u8, height: i32) -> i32 {
        self.embedded_faces()
            .get(variant)
            .as_scaled(height.max(1) as f32)
            .ascent()
            .round() as i32
    }

    /// One glyph of `variant` at `height` pixels, rasterised from the embedded
    /// face and cached: a guest draws a string per line and the credits draw
    /// hundreds, so each glyph is built once and blitted many times. A
    /// character the face has no outline for comes back as the font's `.notdef`.
    fn raster(&mut self, variant: u8, height: i32, character: char) -> std::rc::Rc<RasterGlyph> {
        let key = (variant, height, character);
        if let Some(cached) = self.glyph_cache.get(&key) {
            return std::rc::Rc::clone(cached);
        }
        let scaled = self.embedded_faces().get(variant).as_scaled(height.max(1) as f32);
        let advance = scaled.h_advance(scaled.glyph_id(character)).round() as i32;
        let outlined = scaled
            .outline_glyph(scaled.scaled_glyph(character))
            .map(|outlined| {
                let bounds = outlined.px_bounds();
                let left = bounds.min.x.round() as i32;
                let top = bounds.min.y.round() as i32;
                let width = bounds.width().ceil() as u32;
                let height = bounds.height().ceil() as u32;
                let mut coverage = vec![0_u8; (width * height) as usize];
                outlined.draw(|x, y, c| {
                    let index = (y * width + x) as usize;
                    if index < coverage.len() {
                        coverage[index] = (c * 255.0).round() as u8;
                    }
                });
                RasterGlyph {
                    left,
                    top,
                    width,
                    height,
                    coverage,
                    advance,
                }
            });
        let raster = outlined.unwrap_or_else(|| RasterGlyph {
            left: 1,
            top: 0,
            width: 8,
            height: height.max(8) as u32,
            coverage: {
                // A hollow box, which is what a rasteriser draws for a character
                // it has no outline of rather than dropping it.
                let rows = (height.max(8) as usize).max(8);
                let mut box_rows = vec![0_u8; 8 * rows];
                for row in 0..rows {
                    for column in 0..8 {
                        let edge =
                            row == 0 || row == rows - 1 || column == 0 || column == 7;
                        if edge {
                            box_rows[row * 8 + column] = 255;
                        }
                    }
                }
                box_rows
            },
            advance: 12,
        });
        let raster = std::rc::Rc::new(raster);
        self.glyph_cache.insert(key, std::rc::Rc::clone(&raster));
        raster
    }
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

    /// The bytes the bitmaps this host allocated for themselves hold: the ones
    /// `CreateDIBSection` made, since a resource bitmap's pixels are part of its
    /// module instead. Reported when the arena runs out, because a guest that
    /// loads pictures is the one that fills it.
    pub fn pixel_bytes(&self) -> u64 {
        self.bitmaps
            .values()
            .filter(|bitmap| bitmap.owned.is_some())
            .map(pixel_bytes)
            .sum()
    }

    /// The same figure as a running pair, `(created, released)`: a guest that
    /// draws from what it loaded never releases any of it, which is why its
    /// footprint is the sum of both decodes rather than the last one.
    pub fn pixel_totals(&self) -> (u64, u64) {
        (self.pixel_bytes_created, self.pixel_bytes_released)
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
    let weight = call.arg(4)? as i32;
    Ok(make_font(host, call.arg(0)? as i32, weight, face))
}

/// `CreateFontW`: the same `LOGFONT`, with the face name as UTF-16.
fn create_font_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let face = call
        .arg(13)
        .ok()
        .filter(|address| *address != 0)
        .and_then(|address| call.wstr(address).ok())
        .unwrap_or_default();
    Ok(make_font(host, call.arg(0)? as i32, call.arg(4)? as i32, face))
}

/// The font both creators register: one face, tall what the guest asked for,
/// resolved to the embedded face its name and weight name.
fn make_font(host: &mut Host, height: i32, weight: i32, face: String) -> Action {
    let variant = Face::of(&face, weight).0;
    let handle = FONT_HANDLE + host.gdi.next_font * 4;
    host.gdi.next_font += 1;
    host.gdi
        .fonts
        .insert(handle, Font { height, face, variant });
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
            host.gdi.pixel_bytes_released += pixel_bytes(&bitmap);
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
    host.gdi.pixel_bytes_created += u64::from(size);
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
    // The depth the guest asked its display for. A palettised screen holds
    // indices and the page colours them when the frame is shown, so a palette
    // change reaches the pixels already on screen -- which is what a fade is, and
    // what baking the colour at blit time cannot do. A truecolour screen holds
    // the colours themselves and the page reads them as they stand.
    //
    // 24 is refused because a page reads 8, 16 or 32: a mode whose pixels are not
    // laid out the way a page can read them would be shown wrong rather than not
    // at all, so it falls back to 32.
    let bits = match host.ddraw.display_depth() {
        8 | 16 | 32 => host.ddraw.display_depth(),
        _ => 32,
    };
    let pitch = width * u32::from(bits / 8);
    let size = pitch * height;
    let buffer = host.allocate(size, 4)?;
    host.ddraw
        .make_window_surface(buffer, width, height, pitch, size, bits);
    Ok(())
}

/// Gives the window's screen the size the guest just asked its display for.
///
/// The window's context makes the screen on its first `GetDC`, at whatever size
/// the window happened to be then. A guest that sets its display mode afterwards
/// would otherwise draw a 640-wide layout into a 320-wide surface, and the part
/// past the edge is not just missing: it is where the next row of the picture
/// lands, which reads as a frame that has been folded in half.
pub(crate) fn resize_window_screen(host: &mut Host, width: u32, height: u32) -> Result<()> {
    if width == 0 || height == 0 {
        return Ok(());
    }
    let Some(handle) = host.ddraw.window_surface() else {
        // No screen yet: the first `GetDC` makes one at this size.
        return Ok(());
    };
    let Some((_, on_screen_width, on_screen_height, _, _)) = host.ddraw.surface(handle) else {
        return Ok(());
    };
    if on_screen_width == width && on_screen_height == height {
        return Ok(());
    }
    let bits = match host.ddraw.display_depth() {
        8 | 16 | 32 => host.ddraw.display_depth(),
        _ => 32,
    };
    let pitch = width * u32::from(bits / 8);
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
        colours: bitmap_colours(host, bitmap),
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

/// The value a raster operation reads from one device pixel.
///
/// A raster op is bitwise arithmetic on the device's own pixel values -- the
/// palette index on an 8-bit surface, the packed 16-bit colour on a 16-bit one --
/// so it has to see the bytes, not the colour they stand for.
fn read_raw(memory: &GuestMemory, device: &Device, x: i32, y: i32) -> Result<Option<u32>> {
    if !device.holds(x, y) {
        return Ok(None);
    }
    let row = device.buffer + device.pitch * device.row(y);
    let column = x as u32;
    let value = match device.bits {
        8 => u32::from(memory.read_u8(row + column)?),
        16 => u32::from(memory.read_u16(row + column * 2)?),
        24 => {
            let at = row + column * 3;
            u32::from(memory.read_u8(at)?)
                | u32::from(memory.read_u8(at + 1)?) << 8
                | u32::from(memory.read_u8(at + 2)?) << 16
        }
        _ => memory.read_u32(row + column * 4)?,
    };
    Ok(Some(value))
}

/// Writes the value a raster operation produced back into a device pixel.
fn write_raw(
    memory: &mut GuestMemory,
    device: &Device,
    x: i32,
    y: i32,
    value: u32,
) -> Result<()> {
    if !device.holds(x, y) {
        return Ok(());
    }
    let row = device.buffer + device.pitch * device.row(y);
    let column = x as u32;
    match device.bits {
        8 => memory.write_u8(row + column, value as u8),
        16 => memory.write_u16(row + column * 2, value as u16),
        24 => {
            let at = row + column * 3;
            memory.write_u8(at, value as u8)?;
            memory.write_u8(at + 1, (value >> 8) as u8)?;
            memory.write_u8(at + 2, (value >> 16) as u8)
        }
        _ => memory.write_u32(row + column * 4, value),
    }
}

/// The bits one pixel of a device occupies, so an inversion cannot spill into
/// the pixel beside it.
fn pixel_mask(bits: u8) -> u32 {
    match bits {
        8 => 0xff,
        16 => 0xffff,
        24 => 0x00ff_ffff,
        _ => u32::MAX,
    }
}

/// Composes one destination pixel from one source pixel under `rop`.
fn rop_pixel(
    memory: &mut GuestMemory,
    source: &Device,
    dest: &Device,
    rop: u32,
    at: (i32, i32, i32, i32),
) -> Result<()> {
    let (source_x, source_y, dest_x, dest_y) = at;
    let Some(from) = read_raw(memory, source, source_x, source_y)? else {
        // A source pixel outside the device is left alone, exactly as an
        // unkeyed copy at an edge does.
        return Ok(());
    };
    if !dest.holds(dest_x, dest_y) {
        return Ok(());
    }
    let onto = read_raw(memory, dest, dest_x, dest_y)?.unwrap_or(0);
    // The op combines pixels of one format: Windows converts a source of another
    // depth to the destination's before it runs. Combining a palette index with a
    // 32-bit colour instead -- `0xff & 0x00ffffff` -- paints noise across the
    // whole bitmap, and ANDing a monochrome mask with 1 clears nearly every pixel
    // of the sprite. A 1-bit source is black and white, not the indices 0 and 1.
    let from = if source.bits == dest.bits {
        from
    } else if source.bits == 1 {
        if from != 0 { pixel_mask(dest.bits) } else { 0 }
    } else {
        let Some((red, green, blue)) = read_pixel(memory, source, source_x, source_y)? else {
            return Ok(());
        };
        match dest.bits {
            8 => u32::from(nearest(&dest.colours, (red, green, blue))),
            16 => u32::from(pack_565(red, green, blue)),
            _ => u32::from(red) << 16 | u32::from(green) << 8 | u32::from(blue),
        }
    };
    let value = match rop {
        SRCAND => from & onto,
        SRCPAINT => from | onto,
        SRCINVERT => from ^ onto,
        NOTSRCCOPY => !from,
        SRCERASE => from & !onto,
        _ => from,
    } & pixel_mask(dest.bits);
    write_raw(memory, dest, dest_x, dest_y, value)
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
    if width <= 0 || height <= 0 {
        return Ok(Return(1));
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
    if !ROP_MASKING.contains(&rop) {
        return Err(format!(
            "BitBlt with raster op {rop:#010x}: this host copies (SRCCOPY), masks (SRCAND, \
             SRCPAINT, SRCERASE), inverts (SRCINVERT, NOTSRCCOPY) and fills (BLACKNESS, \
             WHITENESS) and no other op"
        ));
    }
    let Ok(source) = device_of(host, source_dc) else {
        return Ok(Return(0));
    };
    if width <= 0 || height <= 0 {
        return Ok(Return(1));
    }
    // A masking op is pixel arithmetic, so it never takes the row-move path.
    if rop != SRCCOPY {
        for row in 0..height {
            for column in 0..width {
                rop_pixel(
                    call.memory,
                    &source,
                    &dest.device,
                    rop,
                    (
                        source_x + column,
                        source_y + row,
                        dest_x + column,
                        dest_y + row,
                    ),
                )?;
            }
        }
        if let Some(surface) = dest.surface() {
            host.ddraw.touch(surface);
        }
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

/// One `StretchBlt` rectangle as a corner, a size and which ways to flip.
///
/// A negative size means the rectangle runs backwards from the corner given, so
/// `w = -4` at `x = 4` covers columns `0..4`. The size comes back positive, the
/// corner is the rectangle's left/top edge, and the flip says whether the copy
/// reads its source that way round.
fn oriented(x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32, bool, bool) {
    let flip_x = width < 0;
    let flip_y = height < 0;
    (
        if flip_x {
            x.saturating_add(width)
        } else {
            x
        },
        if flip_y {
            y.saturating_add(height)
        } else {
            y
        },
        width.unsigned_abs() as i32,
        height.unsigned_abs() as i32,
        flip_x,
        flip_y,
    )
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
    if !ROP_MASKING.contains(&rop) {
        return Err(format!(
            "StretchBlt with raster op {rop:#010x}: this host copies (SRCCOPY), masks (SRCAND, \
             SRCPAINT, SRCERASE), inverts (SRCINVERT, NOTSRCCOPY) and fills (BLACKNESS, \
             WHITENESS) and no other op"
        ));
    }
    let Ok(source) = device_of(host, source_dc) else {
        if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
            host.notice(format!("StretchBlt source {source_dc:#x} has no device"));
        }
        return Ok(Return(0));
    };
    if crate::shim::switch(&SURFACE_LOGGING, "SHIMKIT_LOG_GDI_SURFACE") {
        // Which surface a scaled copy lands on, and what each side is: a surface
        // that stays black either never got a copy or got one from a source the
        // pixel reader could not read.
        let corner = read_pixel(call.memory, &source, 0, 0)
            .ok()
            .flatten()
            .map_or("?".to_string(), |(r, g, b)| format!("{r:02x}{g:02x}{b:02x}"));
        // How much of the source is actually painted: a scaled copy that lands
        // black either had nothing to copy or dropped what it had.
        let mut lit = 0;
        let mut sampled = 0;
        for row in 0..16 {
            for column in 0..16 {
                let x = source_w * column / 16;
                let y = source_h * row / 16;
                sampled += 1;
                if let Ok(Some((r, g, b))) = read_pixel(call.memory, &source, x, y) {
                    if r != 0 || g != 0 || b != 0 {
                        lit += 1;
                    }
                }
            }
        }
        host.notice(format!(
            "StretchBlt to surface {:#010x} ({dest_w}x{dest_h}) from {source_w}x{source_h} \
             source {}bit ({} colours, corner {corner}, {lit}/{sampled} lit) dest {}bit",
            dest.surface().unwrap_or(0),
            source.bits,
            source.colours.len(),
            dest.device.bits
        ));
    }
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI")
        && host.gdi.dcs.get(&dest_dc).map_or(false, |context| context.window)
    {
        host.notice(format!(
            "StretchBlt to window dest {dest_dc:#x} at {dest_x},{dest_y} {dest_w}x{dest_h} \
             from {source_dc:#x} {source_w}x{source_h}"
        ));
    }
    // A negative extent is a mirror, not an empty rectangle: Windows reads the
    // rectangle as running the other way from the corner it was given, and the
    // copy comes back flipped. That is how a sprite facing left is drawn -- the
    // game keeps one bitmap per facing and blits it backwards -- so the two
    // rectangles are normalised here and the flip carried into the sampling
    // below. A negative size on *both* sides is a straight copy, because the two
    // flips cancel, which is why the copy flips where the rectangles disagree.
    let (dest_x, dest_y, dest_w, dest_h, dest_flip_x, dest_flip_y) =
        oriented(dest_x, dest_y, dest_w, dest_h);
    let (source_x, source_y, source_w, source_h, source_flip_x, source_flip_y) =
        oriented(source_x, source_y, source_w, source_h);
    let (mirror_x, mirror_y) = (dest_flip_x != source_flip_x, dest_flip_y != source_flip_y);
    if dest_w == 0 || dest_h == 0 || source_w == 0 || source_h == 0 {
        return Ok(Return(1));
    }
    // At one for one, a copy between two 32-bit devices is a row move -- and a
    // menu that is composed on a surface and then put up is exactly that.
    if !mirror_x
        && !mirror_y
        && source_w == dest_w
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
        let down = (row as i64 * i64::from(source_h) / i64::from(dest_h)) as i32;
        let source_row = if mirror_y {
            source_y + source_h - 1 - down
        } else {
            source_y + down
        };
        for column in 0..dest_w {
            let across = (column as i64 * i64::from(source_w) / i64::from(dest_w)) as i32;
            let source_column = if mirror_x {
                source_x + source_w - 1 - across
            } else {
                source_x + across
            };
            if rop != SRCCOPY {
                rop_pixel(
                    call.memory,
                    &source,
                    &dest.device,
                    rop,
                    (
                        source_column,
                        source_row,
                        dest_x + column,
                        dest_y + row,
                    ),
                )?;
                continue;
            }
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
    if crate::shim::switch(&SURFACE_LOGGING, "SHIMKIT_LOG_GDI_SURFACE") {
        // What the copy left behind, read back through the same pixel reader: a
        // surface that is still black here had its pixels written somewhere else.
        let mut lit = 0;
        let mut sampled = 0;
        for row in 0..16 {
            for column in 0..16 {
                let x = dest_w * column / 16;
                let y = dest_h * row / 16;
                sampled += 1;
                if let Ok(Some((r, g, b))) =
                    read_pixel(call.memory, &dest.device, dest_x + x, dest_y + y)
                {
                    if r != 0 || g != 0 || b != 0 {
                        lit += 1;
                    }
                }
            }
        }
        host.notice(format!(
            "  after the copy surface {:#010x} at {:#010x} reads {lit}/{sampled} lit",
            dest.surface().unwrap_or(0),
            dest.device.buffer
        ));
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

// ---- palettes, caps, clipping and map modes -------------------------------

/// Whether a point falls outside the rectangle a context draws inside.
fn clipped(clip: [i32; 4], x: i32, y: i32) -> bool {
    x < clip[0] || y < clip[1] || x >= clip[2] || y >= clip[3]
}

/// Whether a rectangle holds no pixels at all.
fn empty(rect: [i32; 4]) -> bool {
    rect[2] <= rect[0] || rect[3] <= rect[1]
}

/// The overlap of two rectangles, which is empty when they miss each other.
fn intersect(first: [i32; 4], second: [i32; 4]) -> [i32; 4] {
    [
        first[0].max(second[0]),
        first[1].max(second[1]),
        first[2].min(second[2]),
        first[3].min(second[3]),
    ]
}

/// A `RECT` from guest memory: four signed `LONG`s, left, top, right, bottom.
fn read_rect(memory: &GuestMemory, at: u32) -> Result<[i32; 4]> {
    Ok([
        memory.read_u32(at)? as i32,
        memory.read_u32(at + 4)? as i32,
        memory.read_u32(at + 8)? as i32,
        memory.read_u32(at + 12)? as i32,
    ])
}

/// Writes the two `LONG`s of a `POINT`, `SIZE` or `SIZEL` the call hands in as an
/// out parameter. A null one is how a guest asks not to be told.
fn write_pair(call: &mut Call<'_>, at: u32, first: i32, second: i32) -> Result<()> {
    if at == 0 {
        return Ok(());
    }
    call.write_u32(at, first as u32)?;
    call.write_u32(at + 4, second as u32)
}

/// The text a call points at: `count` bytes, or up to the terminator when `count`
/// is negative, which is how GDI spells "the whole string".
fn read_text(memory: &GuestMemory, address: u32, count: i32) -> Result<String> {
    if count < 0 {
        return memory.read_cstr(address);
    }
    let bytes = memory.read_bytes(address, count as usize)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

/// The colour table a bitmap created without one indexes with: a greyscale ramp,
/// which is what `CreateBitmap` hands a palettised bitmap it made itself.
fn grey_table(bits: u16) -> Vec<[u8; 3]> {
    let count = 1_usize << bits;
    (0..count)
        .map(|index| {
            let level = (index * 255 / (count - 1).max(1)) as u8;
            [level, level, level]
        })
        .collect()
}

/// The colour table a bitmap is read through.
///
/// A device-dependent bitmap holds indices into the palette of the device that
/// drew it, so it is read through the screen's table -- the one current when it is
/// drawn, not the one it had when it was made, because a game builds its artwork
/// before it sets its palette. A bitmap with a table of its own keeps that table,
/// and a monochrome one keeps black and white.
fn bitmap_colours(host: &Host, bitmap: &Bitmap) -> Vec<[u8; 3]> {
    if bitmap.bits <= 8 && bitmap.colours.is_empty() {
        return host.ddraw.palette().to_vec();
    }
    bitmap.colours.clone()
}

/// A scaled extent: the numerator and denominator of a `Scale...ExtEx` call
/// applied to a value. A zero on either side leaves the extent as it was, which
/// is Windows' own answer when a guest passes nothing to scale by.
fn scaled(value: i32, numerator: u32, denominator: u32) -> i32 {
    if numerator == 0 || denominator == 0 {
        return value;
    }
    (i64::from(value) * i64::from(numerator) / i64::from(denominator)) as i32
}

/// `CreatePalette(LOGPALETTE*)`: a palette the guest fills and then selects into
/// a context.
///
/// The structure is the version word, a count, and one `PALETTEENTRY` per colour:
/// red, green, blue, flags. That is the opposite order to a `BITMAPINFO` colour
/// table, whose `RGBQUAD` starts at blue, so these entries are read with
/// `read_palette_entries` rather than `read_colours`.
fn create_palette(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let source = call.arg(0)?;
    if source == 0 {
        return Ok(Return(0));
    }
    let version = call.memory.read_u16(source)?;
    if version != PALETTE_VERSION {
        return Err(format!(
            "CreatePalette was handed a {version:#06x} LOGPALETTE; this host reads the 0x0300 \
             structure"
        ));
    }
    let count = call.memory.read_u16(source + 2)?;
    let mut palette = Palette::new();
    let entries = read_palette_entries(call.memory, source + 4, count as usize)?;
    palette.write(0, &entries);
    let handle = FIRST_PALETTE + host.gdi.next_palette * 4;
    host.gdi.next_palette += 1;
    host.gdi.palettes.insert(handle, palette);
    Ok(Return(handle))
}

/// `SetPaletteEntries(hpal, iStart, cEntries, entries)`: writes a run of entries,
/// answering how many it took.
///
/// This is the whole of a screen fade in a game of this era: the guest rewrites
/// the table and realizes it with no blit in between, so the entries have to stay
/// addressable by index until `RealizePalette` copies them onto the device.
fn set_palette_entries(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let start = call.arg(1)? as usize;
    let count = call.arg(2)? as usize;
    let source = call.arg(3)?;
    let Some(palette) = host.gdi.palettes.get_mut(&handle) else {
        return Ok(Return(0));
    };
    if source == 0 {
        return Ok(Return(0));
    }
    let entries = read_palette_entries(call.memory, source, count)?;
    palette.write(start, &entries);
    Ok(Return(count as u32))
}

/// `SelectPalette(hdc, hpal, forceBackground)`: the palette the context will
/// realize, with the one it had as the answer.
fn select_palette(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let palette = call.arg(1)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    Ok(Return(context.palette.replace(palette).unwrap_or(0)))
}

/// `RealizePalette(hdc)`: writes the selected palette onto the device the context
/// draws on, and answers how many entries actually changed.
///
/// The count is what a guest watches: a fade realizes the same entries with new
/// colours every frame, and the ones that changed are what a page has to repaint
/// for. The write goes through the display's own table, because that is the one
/// the presented surface is decoded with -- a palette realized nowhere else would
/// leave the screen the colour it was.
fn realize_palette(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let Some(handle) = host.gdi.dcs.get(&dc).and_then(|context| context.palette) else {
        return Ok(Return(0));
    };
    let Some(palette) = host.gdi.palettes.get(&handle) else {
        return Ok(Return(0));
    };
    if palette.first == NO_INDEX || palette.entries.is_empty() {
        return Ok(Return(0));
    }
    let (first, entries) = (palette.first, palette.entries.clone());
    let changed = host.ddraw.write_palette_entries(first as u32, &entries);
    // Realising a palette over the surface the page shows is a frame like any
    // other: there is no flip to count, so the touched surface is what the page
    // notices.
    if let Ok(device) = device_of(host, dc) {
        if let Some(surface) = device.surface {
            host.ddraw.touch(surface);
        }
    }
    Ok(Return(changed))
}

/// `UnrealizeObject(hpal)`: nothing to do, because a palette here is the device's
/// own table rather than one a driver has to be told to stop using.
fn unrealize_object(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `GetDeviceCaps(hdc, index)`: what the device can do.
///
/// The numbers are the real ones: a game of this era asks for the depth and the
/// colour count to decide whether it is drawing in eight bits, and asks for the
/// resolution to lay out a screen.
fn get_device_caps(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let index = call.arg(1)?;
    let bits = device_of(host, dc).map_or(8, |device| device.bits);
    let (width, height) = (host.ddraw.width(), host.ddraw.height());
    let answer = match index {
        HORZRES => width,
        VERTRES => height,
        BITSPIXEL => u32::from(bits),
        PLANES => 1,
        NUMCOLORS => 1_u32 << u32::from(bits).min(31),
        RASTERCAPS => RC_BITBLT,
        LOGPIXELSX | LOGPIXELSY => DOTS_PER_INCH,
        SIZEPALETTE => {
            if bits == 8 {
                256
            } else {
                0
            }
        }
        _ => 0,
    };
    Ok(Return(answer))
}

/// `GetTextExtentPointA(hdc, string, count, size)`: the width and height a run of
/// text occupies in the context's font.
///
/// The width is measured with the same glyph advances the drawing call uses, so a
/// centred line is centred: a guest that measured with one font and drew with
/// another would drift half a word to the right.
fn get_text_extent_point(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let string = call.arg(1)?;
    let count = call.arg(2)? as i32;
    let size = call.arg(3)?;
    let text = read_text(call.memory, string, count)?;
    let (width, height) = text_extent(host, dc, &text);
    write_pair(call, size, width, height)?;
    Ok(Return(1))
}

/// `ExtTextOutA(hdc, x, y, options, rect, string, count, spacings)`.
///
/// Two options matter to a game of this era: `ETO_OPAQUE` fills the rectangle
/// before drawing (which the context's opaque background mode already does for
/// the cell) and `ETO_CLIPPED` narrows the clip for this call alone -- a scrolled
/// list draws every row through one rectangle and expects the rows above and
/// below it to be cut. The per-character `spacings` array is ignored: this host
/// has one face, and a guest that passes one has already laid the text out.
fn ext_text_out(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let options = call.arg(3)?;
    let rectangle = call.arg(4)?;
    let string = call.arg(5)?;
    let count = call.arg(6)? as i32;
    let text = read_text(call.memory, string, count)?;
    if !host.gdi.dcs.contains_key(&dc) {
        return Ok(Return(0));
    }
    let previous = host.gdi.dcs.get(&dc).and_then(|context| context.clip);
    if options & ETO_CLIPPED != 0 && rectangle != 0 {
        let asked = read_rect(call.memory, rectangle)?;
        let narrowed = match previous {
            Some(clip) => intersect(clip, asked),
            None => asked,
        };
        if let Some(context) = host.gdi.dcs.get_mut(&dc) {
            context.clip = Some(narrowed);
        }
    }
    draw_run(host, call.memory, dc, x, y, &text)?;
    if let Some(context) = host.gdi.dcs.get_mut(&dc) {
        context.clip = previous;
    }
    Ok(Return(1))
}

/// `GetClipBox(hdc, rect)`: the part of the device the context may draw on, and
/// how complicated that region is.
fn get_clip_box(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let destination = call.arg(1)?;
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(REGION_ERROR));
    };
    let clip = clip_of(host, dc, &device);
    if destination != 0 {
        call.write_u32(destination, clip[0] as u32)?;
        call.write_u32(destination + 4, clip[1] as u32)?;
        call.write_u32(destination + 8, clip[2] as u32)?;
        call.write_u32(destination + 12, clip[3] as u32)?;
    }
    Ok(Return(if empty(clip) { NULLREGION } else { SIMPLEREGION }))
}

/// `IntersectClipRect(hdc, left, top, right, bottom)`: narrows the context's clip
/// and answers the complexity of what is left.
///
/// This is one rectangle rather than the list of bands Windows keeps, and that is
/// enough because every game here intersects with a plain box: a panel, or the
/// list of a scroll. A second intersection coming out empty is what tells the
/// guest that nothing it draws will be seen.
fn intersect_clip_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let asked = [
        call.arg(1)? as i32,
        call.arg(2)? as i32,
        call.arg(3)? as i32,
        call.arg(4)? as i32,
    ];
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(REGION_ERROR));
    };
    let whole = [0, 0, device.width, device.height];
    let current = host
        .gdi
        .dcs
        .get(&dc)
        .and_then(|context| context.clip)
        .unwrap_or(whole);
    let narrowed = intersect(current, intersect(asked, whole));
    if let Some(context) = host.gdi.dcs.get_mut(&dc) {
        context.clip = Some(narrowed);
    }
    Ok(Return(if empty(narrowed) {
        NULLREGION
    } else if narrowed == whole {
        SIMPLEREGION
    } else {
        COMPLEXREGION
    }))
}

/// `PtVisible(hdc, x, y)`: whether a point is inside what the context may draw on.
fn pt_visible(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(u32::MAX));
    };
    let clip = clip_of(host, dc, &device);
    Ok(Return(u32::from(!clipped(clip, x, y))))
}

/// `RectVisible(hdc, rect)`: whether any part of a rectangle would be drawn.
fn rect_visible(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let rectangle = call.arg(1)?;
    if rectangle == 0 {
        return Ok(Return(0));
    }
    let asked = read_rect(call.memory, rectangle)?;
    let Ok(device) = device_of(host, dc) else {
        return Ok(Return(u32::MAX));
    };
    let clip = clip_of(host, dc, &device);
    Ok(Return(u32::from(!empty(intersect(clip, asked)))))
}

/// `SaveDC(hdc)`: pushes a copy of the context's state, answering the level it is
/// at.
fn save_dc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let state = context.clone();
    context.saved.push(state);
    Ok(Return(context.saved.len() as u32))
}

/// `RestoreDC(hdc, level)`: puts a saved state back.
///
/// A negative level means "the most recent", which is how a guest that saved one
/// context around a piece of drawing calls it; a positive one names the level
/// and discards everything saved above it, as Windows does.
fn restore_dc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let level = call.arg(1)? as i32;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    if level < 0 {
        let Some(state) = context.saved.pop() else {
            return Ok(Return(0));
        };
        *context = state;
        return Ok(Return(1));
    }
    let level = level as usize;
    if level == 0 || level > context.saved.len() {
        return Ok(Return(0));
    }
    let state = context.saved[level - 1].clone();
    *context = state;
    Ok(Return(1))
}

/// `SetMapMode(hdc, mode)`: the previous mode, or zero for a context that is not
/// there.
///
/// Only `MM_TEXT` -- one device unit to one pixel -- is given meaning. The other
/// modes are recorded and answered back so a guest that saves and restores a
/// context sees its own value, but no call is scaled through them: every game
/// this host runs sets `MM_TEXT` and computes its own coordinates.
fn set_map_mode(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let mode = call.arg(1)? as i32;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    Ok(Return(
        std::mem::replace(&mut context.map_mode, mode) as u32
    ))
}

/// `SetWindowExtEx(hdc, x, y, previous)`: the logical extent, with the one it had
/// written back.
fn set_window_ext(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let previous_at = call.arg(3)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = std::mem::replace(&mut context.window_ext, (x, y));
    write_pair(call, previous_at, previous.0, previous.1)?;
    Ok(Return(1))
}

/// `SetViewportExtEx(hdc, x, y, previous)`: the device extent, with the one it had
/// written back.
fn set_viewport_ext(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let previous_at = call.arg(3)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = std::mem::replace(&mut context.viewport_ext, (x, y));
    write_pair(call, previous_at, previous.0, previous.1)?;
    Ok(Return(1))
}

/// `SetViewportOrgEx(hdc, x, y, previous)`: where the viewport's origin sits, with
/// the one it had written back.
fn set_viewport_org(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (x, y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let previous_at = call.arg(3)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = std::mem::replace(&mut context.viewport_org, (x, y));
    write_pair(call, previous_at, previous.0, previous.1)?;
    Ok(Return(1))
}

/// `OffsetViewportOrgEx(hdc, dx, dy, previous)`: moves the viewport's origin by a
/// step, answering where it was.
fn offset_viewport_org(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (dx, dy) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let previous_at = call.arg(3)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = context.viewport_org;
    context.viewport_org = (
        previous.0.saturating_add(dx),
        previous.1.saturating_add(dy),
    );
    write_pair(call, previous_at, previous.0, previous.1)?;
    Ok(Return(1))
}

/// `ScaleWindowExtEx(hdc, xn, xd, yn, yd, previous)`: multiplies the logical
/// extent by a ratio, writing back what it was.
fn scale_window_ext(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (xn, xd) = (call.arg(1)?, call.arg(2)?);
    let (yn, yd) = (call.arg(3)?, call.arg(4)?);
    let previous_at = call.arg(5)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = context.window_ext;
    context.window_ext = (
        scaled(previous.0, xn, xd),
        scaled(previous.1, yn, yd),
    );
    write_pair(call, previous_at, previous.0, previous.1)?;
    Ok(Return(1))
}

/// `ScaleViewportExtEx(hdc, xn, xd, yn, yd, previous)`: multiplies the device
/// extent by a ratio, writing back what it was.
fn scale_viewport_ext(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (xn, xd) = (call.arg(1)?, call.arg(2)?);
    let (yn, yd) = (call.arg(3)?, call.arg(4)?);
    let previous_at = call.arg(5)?;
    let Some(context) = host.gdi.dcs.get_mut(&dc) else {
        return Ok(Return(0));
    };
    let previous = context.viewport_ext;
    context.viewport_ext = (
        scaled(previous.0, xn, xd),
        scaled(previous.1, yn, yd),
    );
    write_pair(call, previous_at, previous.0, previous.1)?;
    Ok(Return(1))
}

/// `CreateBitmap(width, height, planes, bits, pixels)`: a bitmap GDI owns.
///
/// A guest that builds a picture pixel by pixel -- a shadowed title, a dithered
/// gradient -- makes one of these, writes into the pixels it is given and blits
/// the handle. The pixels are the guest's own buffer when it passed one, and an
/// arena block when it did not, which `DeleteObject` gives back.
fn create_bitmap(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let width = call.arg(0)? as i32;
    let height = call.arg(1)? as i32;
    let planes = call.arg(2)?;
    let bits = call.arg(3)? as u16;
    let source = call.arg(4)?;
    if width <= 0 || height <= 0 || planes != 1 || !matches!(bits, 1 | 4 | 8 | 16 | 24 | 32) {
        return Ok(Return(0));
    }
    let size = stride_of(width, bits)
        .checked_mul(height as u32)
        .ok_or_else(|| format!("CreateBitmap was handed a {width}x{height} bitmap that is too large"))?;
    let pixels = if source != 0 {
        source
    } else {
        host.allocate(size, 4)?
    };
    let handle = FIRST_BITMAP + 0x1000 + host.gdi.next_bitmap * 4;
    host.gdi.next_bitmap += 1;
    host.gdi.bitmaps.insert(
        handle,
        Bitmap {
            width,
            height,
            bits,
            pixels,
            // A device-dependent bitmap takes its colours from the device it is
            // made for, so a palettised one carries no table of its own and is
            // read through the screen's palette when it is drawn. A monochrome
            // one is the exception: black and white is what a mask depends on.
            colours: if bits == 1 { grey_table(1) } else { Vec::new() },
            owned: (source == 0).then_some(pixels),
        },
    );
    Ok(Return(handle))
}

/// `CreateDIBitmap(hdc, header, init, bits, info, usage)`: a bitmap described by a
/// `BITMAPINFOHEADER`, holding either the guest's pixels or a block of its own.
fn create_di_bitmap(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let header = call.arg(1)?;
    let init = call.arg(2)?;
    let source = call.arg(3)?;
    let colours_at = call.arg(4)?;
    if header == 0 {
        return Ok(Return(0));
    }
    let header_size = call.memory.read_u32(header)?;
    if header_size < BITMAPINFOHEADER {
        return Err(format!(
            "CreateDIBitmap was handed a {header_size}-byte header, which is not a \
             BITMAPINFOHEADER"
        ));
    }
    let width = call.memory.read_u32(header + 4)? as i32;
    let height = call.memory.read_u32(header + 8)? as i32;
    let planes = call.memory.read_u16(header + 12)?;
    let bits = call.memory.read_u16(header + 14)?;
    if width <= 0 || height == 0 || planes != 1 || !matches!(bits, 1 | 4 | 8 | 16 | 24 | 32) {
        return Ok(Return(0));
    }
    let supplied = init & CBM_INIT != 0 && source != 0;
    let size = stride_of(width, bits)
        .checked_mul(height.unsigned_abs())
        .ok_or_else(|| {
            format!("CreateDIBitmap was handed a {width}x{height} bitmap that is too large")
        })?;
    let pixels = if supplied {
        source
    } else {
        host.allocate(size, 4)?
    };
    let entries = if bits <= 8 { 1_usize << bits } else { 0 };
    let colours = if colours_at != 0 {
        let table = colours_at.saturating_add(header_size);
        read_colours(call.memory, table, entries)?
    } else if bits == 1 {
        grey_table(1)
    } else {
        // No table supplied: a device-dependent bitmap, read through the
        // screen's palette rather than a grey ramp of its own.
        Vec::new()
    };
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
            owned: (!supplied).then_some(pixels),
        },
    );
    Ok(Return(handle))
}

/// `StretchDIBits(dc, ...)`: copies a device-independent bitmap from ordinary
/// memory onto a context, stretched or one pixel for one, and answers the scan
/// lines it copied.
///
/// A game that mixes its own pixel work with GDI draws a whole screen this way:
/// the bitmap is a plain buffer with a `BITMAPINFO` beside it, so the source is
/// read as a device and each destination pixel takes the nearest source pixel --
/// which is what a stretch to the same size is, exactly. The raster operation is
/// the twelfth argument and is not read: only `SRCCOPY` is ever asked for here.
fn stretch_dibits(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dc = call.arg(0)?;
    let (dest_x, dest_y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let (dest_width, dest_height) = (call.arg(3)? as i32, call.arg(4)? as i32);
    let (source_x, source_y) = (call.arg(5)? as i32, call.arg(6)? as i32);
    let (source_width, source_height) = (call.arg(7)? as i32, call.arg(8)? as i32);
    let bits_at = call.arg(9)?;
    let info = call.arg(10)?;
    // A zero destination size means "the source's own": Windows reads it that
    // way, and it is how a launcher draws the 320x240 picture it decoded one
    // pixel for one. Refusing it dropped every frame such a guest ever drew.
    let (dest_width, dest_height) = if dest_width <= 0 || dest_height <= 0 {
        (source_width, source_height)
    } else {
        (dest_width, dest_height)
    };
    if dest_width <= 0 || dest_height <= 0 || source_width <= 0 || source_height <= 0 {
        if std::env::var("SHIMKIT_LOG_GDI").is_ok() {
            host.notice(format!(
                "StretchDIBits refused: dest {dest_width}x{dest_height} at {dest_x},{dest_y}, \
                 source {source_width}x{source_height} at {source_x},{source_y}"
            ));
        }
        return Ok(Return(0));
    }
    if info == 0 || bits_at == 0 {
        if std::env::var("SHIMKIT_LOG_GDI").is_ok() {
            host.notice(format!(
                "StretchDIBits refused: pixels {bits_at:#010x}, BITMAPINFO {info:#010x}"
            ));
        }
        return Ok(Return(0));
    }
    let header_size = call.memory.read_u32(info)?;
    if header_size < BITMAPINFOHEADER {
        return Err(format!(
            "StretchDIBits was handed a {header_size}-byte header, which is not a \
             BITMAPINFOHEADER"
        ));
    }
    // The stride comes from the bitmap's own width, not the rectangle being
    // copied: a guest that copies a strip out of a wider picture still reads rows
    // of the picture's pitch.
    let bitmap_width = call.memory.read_u32(info + 4)? as i32;
    let bitmap_height = call.memory.read_u32(info + 8)? as i32;
    let bits = call.memory.read_u16(info + 14)?;
    let entries = if bits <= 8 { 1_usize << bits } else { 0 };
    // The device describes the whole bitmap, not the rectangle being copied.
    // `row` turns a picture row into a memory row with the device's height, so a
    // device sized to the rectangle rolls every row by the height difference,
    // and `holds` would clip the columns outside the rectangle: a strip copied
    // out of a wider picture then lands sheared instead of offset. A guest that
    // leaves the header's size at zero falls back to the rectangle it asked for.
    let (bitmap_width, bitmap_height) = if bitmap_width > 0 && bitmap_height != 0 {
        (bitmap_width, bitmap_height)
    } else {
        (source_width, source_height)
    };
    let source = Device {
        buffer: bits_at,
        width: bitmap_width,
        height: bitmap_height.abs(),
        pitch: stride_of(bitmap_width, bits),
        bits: bits as u8,
        bottom_up: bitmap_height > 0,
        colours: read_colours(call.memory, info + header_size, entries)?,
        surface: None,
    };
    let Ok(dest) = device_of(host, dc) else {
        if std::env::var("SHIMKIT_LOG_GDI").is_ok() {
            host.notice(format!(
                "StretchDIBits refused: DC {dc:#010x} does not draw on a device this host has"
            ));
        }
        return Ok(Return(0));
    };
    host.notice(format!(
        "DIB bits {bits_at:#010x} {bitmap_width}x{bitmap_height}x{bits} -> dc {dc:#x} at {dest_x},{dest_y} {dest_width}x{dest_height} rop {:#010x}",
        call.arg(12)?
    ));
    let clip = clip_of(host, dc, &dest);
    let mut painter = Painter::new(dest);
    for row in 0..dest_height {
        let target_y = dest_y.saturating_add(row);
        let source_row = source_y.saturating_add(row.saturating_mul(source_height) / dest_height);
        for column in 0..dest_width {
            let target_x = dest_x.saturating_add(column);
            if clipped(clip, target_x, target_y) {
                continue;
            }
            let source_column =
                source_x.saturating_add(column.saturating_mul(source_width) / dest_width);
            if !source.holds(source_column, source_row) {
                continue;
            }
            let Some(colour) = read_pixel(call.memory, &source, source_column, source_row)? else {
                continue;
            };
            painter.put(call.memory, target_x, target_y, colour)?;
        }
    }
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(dest_height as u32))
}

/// `Escape(hdc, code, in, in, out)`: a printer or metafile escape.
///
/// This host has neither a printer nor a metafile driver, so every escape answers
/// zero -- the value Windows gives for a query it cannot serve. A guest that asks
/// `QUERYESCSUPPORT` therefore learns the escape is unsupported, which is the
/// truth, instead of being told yes and then handed nothing.
fn escape(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `GdiFlush()`: there is no batch to flush, because every drawing call here has
/// already written the pixels the page reads. A guest that flushes is waiting for
/// its own drawing to be visible, which it is.
fn gdi_flush(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
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
                variant: Face::MONO,
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
    fn a_stretch_dibits_reads_a_strip_from_the_rows_of_the_whole_bitmap() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_dc(&mut host, &mut memory);
        let screen = host
            .ddraw
            .window_surface()
            .expect("GetDC gave the window a screen");
        // A 4x3 bottom-up 32-bit bitmap, which is what a resource sprite is. The
        // device the copy reads through describes the whole bitmap; sizing it to
        // the rectangle instead rolls every row by the height difference and
        // clips the columns outside the strip, so the sprite arrives sheared.
        let (_, pixels) = dib(&mut host, &mut memory, 4, 3, 32, &[]);
        let info = STACK_BASE + 0x6000;
        // One distinct word per picture pixel: red from the picture row, green
        // from the column.
        let picture = |row: u32, column: u32| {
            ((row + 1) << 20) | ((column + 1) << 8) | 0x11
        };
        for row in 0..3 {
            for column in 0..4 {
                // Bottom-up: the first row in memory is the last picture row.
                memory
                    .write_u32(pixels + (2 - row) * 16 + column * 4, picture(row, column))
                    .unwrap();
            }
        }
        // Copy picture rows 1..3, columns 1..3 into the window's screen.
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "StretchDIBits",
                &[window, 0, 0, 2, 2, 1, 1, 2, 2, pixels, info, 0, SRCCOPY]
            )
            .unwrap(),
            Return(2)
        );
        let (buffer, _, _) = host.ddraw.framebuffer().expect("the guest has a screen");
        let (_, _, _, pitch, _) = host.ddraw.surface(screen).expect("the screen is a surface");
        let onscreen = |x: u32, y: u32| memory.read_u32(buffer + pitch * y + x * 4).unwrap();
        assert_eq!(
            onscreen(0, 0),
            picture(1, 1),
            "the strip's first pixel is picture row 1, column 1"
        );
        assert_eq!(onscreen(1, 0), picture(1, 2));
        assert_eq!(
            onscreen(0, 1),
            picture(2, 1),
            "a second row of the strip is the next picture row, not a rolled one"
        );
        assert_eq!(onscreen(1, 1), picture(2, 2));
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
    fn a_masked_sprite_composes_on_a_palettised_bitmap() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // Hercules and its neighbours draw on 8-bit bitmaps, where a raster op
        // combines palette indices rather than colours: the background is index
        // 2, the mask is 0 where the sprite covers and 0xff elsewhere, and the
        // image holds its own index there.
        let table = [[0, 0, 0], [255, 255, 255]];
        let (dest_handle, dest_pixels) = dib(&mut host, &mut memory, 4, -1, 8, &table);
        for index in 0..4u32 {
            memory.write_u8(dest_pixels + index, 2).unwrap();
        }
        let dest = context(&mut host, &mut memory, dest_handle);
        let (mask_handle, mask_pixels) = dib(&mut host, &mut memory, 4, -1, 8, &table);
        for (index, value) in [0u8, 0, 0xff, 0xff].iter().enumerate() {
            memory.write_u8(mask_pixels + index as u32, *value).unwrap();
        }
        let mask = context(&mut host, &mut memory, mask_handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[dest, 0, 0, 4, 1, mask, 0, 0, SRCAND]
            )
            .unwrap(),
            Return(1)
        );
        let (image_handle, image_pixels) = dib(&mut host, &mut memory, 4, -1, 8, &table);
        for (index, value) in [1u8, 1, 0, 0].iter().enumerate() {
            memory.write_u8(image_pixels + index as u32, *value).unwrap();
        }
        let image = context(&mut host, &mut memory, image_handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[dest, 0, 0, 4, 1, image, 0, 0, SRCPAINT]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u8(dest_pixels).unwrap(), 1, "the sprite's index");
        assert_eq!(memory.read_u8(dest_pixels + 1).unwrap(), 1);
        assert_eq!(
            memory.read_u8(dest_pixels + 2).unwrap(),
            2,
            "the background index outside the mask survives"
        );
        assert_eq!(memory.read_u8(dest_pixels + 3).unwrap(), 2);
    }

    #[test]
    fn a_palettised_mask_reaches_a_truecolour_destination_as_colour() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The screen: four white pixels.
        let (dest_handle, dest_pixels) = dib(&mut host, &mut memory, 4, -1, 32, &[]);
        for index in 0..4u32 {
            memory.write_u32(dest_pixels + index * 4, 0x00ff_ffff).unwrap();
        }
        let dest = context(&mut host, &mut memory, dest_handle);
        // An 8-bit mask whose table says index 1 is white: black where the sprite
        // covers, white where it is transparent. Its indices must reach the op as
        // colours, not as the numbers 0 and 1.
        let table = [[0, 0, 0], [255, 255, 255]];
        let (mask_handle, mask_pixels) = dib(&mut host, &mut memory, 4, -1, 8, &table);
        for (index, value) in [0u8, 0, 1, 1].iter().enumerate() {
            memory.write_u8(mask_pixels + index as u32, *value).unwrap();
        }
        let mask = context(&mut host, &mut memory, mask_handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[dest, 0, 0, 4, 1, mask, 0, 0, SRCAND]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(dest_pixels).unwrap(), 0, "the sprite's pixels are cleared");
        assert_eq!(
            memory.read_u32(dest_pixels + 8).unwrap(),
            0x00ff_ffff,
            "the transparent half stays white, not 0x00000001"
        );
        assert_eq!(memory.read_u32(dest_pixels + 12).unwrap(), 0x00ff_ffff);
    }

    #[test]
    fn a_monochrome_mask_keeps_the_background_outside_the_sprite() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // Four white pixels of background, and a 1-bit mask whose first two
        // pixels are black (the sprite) and last two white (transparent).
        let (dest_handle, dest_pixels) = dib(&mut host, &mut memory, 4, -1, 32, &[]);
        for index in 0..4u32 {
            memory.write_u32(dest_pixels + index * 4, 0x00ff_ffff).unwrap();
        }
        let dest = context(&mut host, &mut memory, dest_handle);
        let (mask_handle, mask_pixels) = dib(&mut host, &mut memory, 4, -1, 1, &[]);
        memory.write_u8(mask_pixels, 0b0011_0000).unwrap();
        let mask = context(&mut host, &mut memory, mask_handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[dest, 0, 0, 4, 1, mask, 0, 0, SRCAND]
            )
            .unwrap(),
            Return(1)
        );
        // Every pixel outside the mask must still be white: a monochrome source
        // that reaches the op as 0 and 1 would leave 0x00000001 instead.
        assert_eq!(memory.read_u32(dest_pixels + 8).unwrap(), 0x00ff_ffff);
        assert_eq!(memory.read_u32(dest_pixels + 12).unwrap(), 0x00ff_ffff);
        assert_eq!(memory.read_u32(dest_pixels).unwrap(), 0, "the mask cleared the sprite's pixels");
    }

    #[test]
    fn a_gdi_sprite_is_masked_with_srcand_then_srcpaint() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The background: four white pixels.
        let (dest_handle, dest_pixels) = dib(&mut host, &mut memory, 4, -1, 32, &[]);
        for index in 0..4u32 {
            memory.write_u32(dest_pixels + index * 4, 0x00ff_ffff).unwrap();
        }
        let dest = context(&mut host, &mut memory, dest_handle);
        // The mask: black where the sprite covers, white where it does not.
        let (mask_handle, mask_pixels) = dib(&mut host, &mut memory, 4, -1, 32, &[]);
        for (index, value) in [0u32, 0, 0x00ff_ffff, 0x00ff_ffff].iter().enumerate() {
            memory
                .write_u32(mask_pixels + index as u32 * 4, *value)
                .unwrap();
        }
        let mask = context(&mut host, &mut memory, mask_handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[dest, 0, 0, 4, 1, mask, 0, 0, SRCAND]
            )
            .unwrap(),
            Return(1),
            "the mask is a raster op this host performs"
        );
        // The image: the sprite's two pixels in red, nothing under the rest.
        let (image_handle, image_pixels) = dib(&mut host, &mut memory, 4, -1, 32, &[]);
        for (index, value) in [0x00ff_0000u32, 0x00ff_0000, 0, 0].iter().enumerate() {
            memory
                .write_u32(image_pixels + index as u32 * 4, *value)
                .unwrap();
        }
        let image = context(&mut host, &mut memory, image_handle);
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "BitBlt",
                &[dest, 0, 0, 4, 1, image, 0, 0, SRCPAINT]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(
            memory.read_u32(dest_pixels).unwrap(),
            0x00ff_0000,
            "the sprite's own pixels are the image"
        );
        assert_eq!(memory.read_u32(dest_pixels + 4).unwrap(), 0x00ff_0000);
        assert_eq!(
            memory.read_u32(dest_pixels + 8).unwrap(),
            0x00ff_ffff,
            "the background outside the mask is untouched"
        );
        assert_eq!(memory.read_u32(dest_pixels + 12).unwrap(), 0x00ff_ffff);
    }

    #[test]
    fn a_palettised_bitmap_reads_through_the_screen_palette() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let window = window_dc(&mut host, &mut memory);
        // A 256-entry palette whose fifth entry no grey ramp can produce.
        let log = STACK_BASE + 0x7000;
        memory.write_u16(log, 0x0300).unwrap();
        memory.write_u16(log + 2, 256).unwrap();
        for index in 0..256u32 {
            memory
                .write_bytes(log + 4 + index * 4, &[0x21, 0x43, 0x65, 0])
                .unwrap();
        }
        memory
            .write_bytes(log + 4 + 5 * 4, &[0x40, 0x80, 0xc0, 0])
            .unwrap();
        let Return(palette) = dispatch(&mut host, &mut memory, "CreatePalette", &[log]).unwrap()
        else {
            panic!("CreatePalette answers with a handle");
        };
        dispatch(
            &mut host,
            &mut memory,
            "SelectPalette",
            &[window, palette, 0],
        )
        .unwrap();
        dispatch(&mut host, &mut memory, "RealizePalette", &[window]).unwrap();
        assert_eq!(
            host.ddraw.palette()[5],
            [0x40, 0x80, 0xc0],
            "the realized palette is the one the screen reads through"
        );

        // A palettised bitmap the guest makes for itself holds indices, not
        // colours: Windows gives it the device's palette, and reading it through
        // a grey ramp paints every sprite as a flat grey silhouette.
        let Return(bitmap) =
            dispatch(&mut host, &mut memory, "CreateBitmap", &[1, 1, 1, 8, 0]).unwrap()
        else {
            panic!("CreateBitmap answers with a handle");
        };
        assert!(
            host.gdi.bitmaps.get(&bitmap).unwrap().colours.is_empty(),
            "a device-dependent bitmap has no table of its own"
        );
        let pixels = host.gdi.bitmaps.get(&bitmap).unwrap().pixels;
        memory.write_u8(pixels, 5).unwrap();
        let dc = context(&mut host, &mut memory, bitmap);
        let device = device_of(&host, dc).unwrap();
        assert_eq!(
            read_pixel(&memory, &device, 0, 0).unwrap(),
            Some((0x40, 0x80, 0xc0)),
            "index 5 reads through the screen palette"
        );
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
    fn a_negative_stretch_extent_mirrors_the_copy() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The shape a sprite facing left takes: one bitmap, blitted backwards.
        let (source, source_pixels) = dib(&mut host, &mut memory, 4, -1, 32, &[]);
        let (target, target_pixels) = dib(&mut host, &mut memory, 8, -1, 32, &[]);
        for column in 0..4 {
            memory
                .write_u32(source_pixels + column * 4, 0x00ff_0000 | (column + 1))
                .unwrap();
        }
        let source_dc = context(&mut host, &mut memory, source);
        let target_dc = context(&mut host, &mut memory, target);
        let read = |memory: &GuestMemory, column: u32| memory.read_u32(target_pixels + column * 4).unwrap();

        // `w = -4` at `x = 4` runs leftwards to column 0 and comes back flipped.
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "StretchBlt",
                &[target_dc, 4, 0, (-4_i32) as u32, 1, source_dc, 0, 0, 4, 1, SRCCOPY]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(read(&memory, 0), 0x00ff_0004, "the rightmost source pixel first");
        assert_eq!(read(&memory, 3), 0x00ff_0001);
        assert_eq!(read(&memory, 4), 0, "nothing is drawn past the corner given");

        // A negative source extent flips too, so the two cancel: a straight copy.
        for column in 0..4 {
            memory.write_u32(target_pixels + column * 4, 0).unwrap();
        }
        assert_eq!(
            dispatch(
                &mut host,
                &mut memory,
                "StretchBlt",
                &[target_dc, 4, 0, (-4_i32) as u32, 1, source_dc, 4, 0, (-4_i32) as u32, 1, SRCCOPY]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(read(&memory, 0), 0x00ff_0001, "two flips are no flip");
        assert_eq!(read(&memory, 3), 0x00ff_0004);
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
