//! The `MSIMG32.dll` blits a game draws its menus with.
//!
//! `AlphaBlend` and `TransparentBlt` are GDI blits with one more rule each: the
//! first mixes the source over the destination, the second drops the pixels that
//! match a key colour. Both sides are devices here, exactly as they are for
//! `BitBlt` -- a surface's context, the window's context, or a bitmap selected into
//! a memory context -- so a program that builds a menu off screen and copies it up
//! with either one lands on the surface the page shows.
//!
//! The destination is 8-bit palettised in the games this host runs, so a blended
//! pixel is found by nearest entry, as GDI does on such a device.

use crate::memory::GuestMemory;
use crate::shim::gdi32::{self, device_of, Painter};
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const MSIMG32: &str = "MSIMG32.dll";

/// `AC_SRC_ALPHA`: the source bitmap's own alpha channel is meaningful. Without
/// it a blend uses only `SourceConstantAlpha`, as `AlphaBlend` on a window DC does.
const AC_SRC_ALPHA: u32 = 0x01;

/// Whether the blend trace is on. A menu blends every frame, so the switch is read
/// once rather than on each call.
static LOGGING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

pub const SHIMS: &[Binding] = &[
    Binding { dll: MSIMG32, name: "AlphaBlend", arg_bytes: 44, shim: alpha_blend },
    Binding { dll: MSIMG32, name: "TransparentBlt", arg_bytes: 44, shim: transparent_blt },
];

/// `AlphaBlend(dest, x, y, w, h, source, sx, sy, sw, sh, blend)`.
///
/// The blend function arrives by value in one word: the operation (always
/// `AC_SRC_OVER` here), a flags byte, the constant alpha, and whether the source's
/// own alpha counts. A pixel's alpha is the constant scaled by the source's, when
/// the source has one.
fn alpha_blend(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
        host.notice(format!(
            "AlphaBlend dest {:#x} ({},{}) {}x{} source {:#x} ({},{}) {}x{} blend {:#x}",
            call.arg(0)?, call.arg(1)?, call.arg(2)?, call.arg(3)?, call.arg(4)?,
            call.arg(5)?, call.arg(6)?, call.arg(7)?, call.arg(8)?, call.arg(9)?,
            call.arg(10)?
        ));
    }
    let dest = device_of(host, call.arg(0)?)?;
    let (dest_x, dest_y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let (width, height) = (call.arg(3)? as i32, call.arg(4)? as i32);
    let source = device_of(host, call.arg(5)?)?;
    let (source_x, source_y) = (call.arg(6)? as i32, call.arg(7)? as i32);
    let (source_width, source_height) = (call.arg(8)? as i32, call.arg(9)? as i32);
    let blend = call.arg(10)?;
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
        let mut lowest = 255_u8;
        let mut highest = 0_u8;
        let step = if source_width * source_height <= 200_000 { 1 } else { 16 };
        for row in (0..source_height).step_by(step as usize) {
            for column in (0..source_width).step_by(step as usize) {
                let alpha = gdi32::read_alpha(
                    call.memory,
                    &source,
                    source_x + column,
                    source_y + row,
                    true,
                )?;
                lowest = lowest.min(alpha);
                highest = highest.max(alpha);
            }
        }
        host.notice(format!(
            "AlphaBlend source {}x{}x{} alpha {lowest}..{highest}",
            source.width, source.height, source.bits
        ));
    }
    let constant = ((blend >> 16) & 0xff) as u32;
    let source_alpha = blend & 0xff00_0000 != 0 && blend & (AC_SRC_ALPHA << 16) != 0;
    if width <= 0 || height <= 0 || source_width <= 0 || source_height <= 0 {
        return Ok(Return(1));
    }
    // A blend of an opaque source onto a 32-bit device at one for one is a copy:
    // every pixel's alpha is 255, so nothing is mixed. A menu composited every
    // frame lands here, and the mix below costs several calls a pixel.
    if constant == 255
        && source_alpha
        && width == source_width
        && height == source_height
        && opaque_32(call.memory, &source, source_x, source_y, width, height)?
        && gdi32::copy_rows_32(
            call.memory,
            &source,
            &dest,
            (source_x, source_y),
            (dest_x, dest_y),
            width,
            height,
        )?
    {
        if let Some(surface) = dest.surface {
            host.ddraw.touch(surface);
        }
        return Ok(Return(1));
    }
    let mut painter = Painter::new(dest);
    for row in 0..height {
        for column in 0..width {
            let x = source_x + column * source_width / width;
            let y = source_y + row * source_height / height;
            let Some((red, green, blue)) = gdi32::read_pixel(call.memory, &source, x, y)? else {
                continue;
            };
            let alpha = u32::from(gdi32::read_alpha(call.memory, &source, x, y, source_alpha)?);
            // `SourceConstantAlpha` scales the source's own alpha, as GDI
            // documents: 255 is "no change" and zero is "nothing drawn".
            let alpha = alpha * constant / 255;
            if alpha == 0 {
                continue;
            }
            let (colour, under) = match gdi32::read_pixel(call.memory, &painter.device, dest_x + column, dest_y + row)? {
                Some((back_red, back_green, back_blue)) if alpha < 255 => {
                    let mix = |front: u8, back: u8| {
                        ((u32::from(front) * alpha + u32::from(back) * (255 - alpha)) / 255) as u8
                    };
                    (
                        (
                            mix(red, back_red),
                            mix(green, back_green),
                            mix(blue, back_blue),
                        ),
                        Some(gdi32::read_alpha(
                            call.memory,
                            &painter.device,
                            dest_x + column,
                            dest_y + row,
                            true,
                        )?),
                    )
                }
                _ => ((red, green, blue), None),
            };
            // A blend into a 32-bit surface carries the result's alpha with it:
            // compositing glyphs onto a transparent panel is what leaves the
            // letters opaque and everything else clear.
            let out_alpha = match under {
                Some(back) => {
                    (alpha + u32::from(back) * (255 - alpha) / 255).min(255) as u8
                }
                None => alpha.min(255) as u8,
            };
            painter.put_with_alpha(
                call.memory,
                dest_x + column,
                dest_y + row,
                colour,
                out_alpha,
            )?;
        }
    }
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(1))
}

/// Whether every pixel of a rectangle of a 32-bit device is opaque.
///
/// A word a pixel, with no colour conversion: the question is only the alpha byte,
/// and answering it is what lets an opaque composite take the copy path.
fn opaque_32(
    memory: &GuestMemory,
    device: &gdi32::Device,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<bool> {
    if device.bits != 32 || width <= 0 || height <= 0 {
        return Ok(false);
    }
    for row in 0..height {
        for column in 0..width {
            let Some(at) = device.address32(x + column, y + row) else {
                return Ok(false);
            };
            if memory.read_u32(at)? >> 24 != 0xff {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// `TransparentBlt(dest, x, y, w, h, source, sx, sy, sw, sh, key)`: a stretched
/// copy that leaves the destination alone wherever the source is the key colour.
///
/// The key is a `COLORREF`, so it is compared as red, green and blue; a source
/// pixel equal to it is not drawn, which is how a sprite with a flat background
/// lands on a menu without a mask.
fn transparent_blt(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
        host.notice(format!(
            "TransparentBlt dest {:#x} ({},{}) {}x{} source {:#x} ({},{}) {}x{}",
            call.arg(0)?, call.arg(1)?, call.arg(2)?, call.arg(3)?, call.arg(4)?,
            call.arg(5)?, call.arg(6)?, call.arg(7)?, call.arg(8)?, call.arg(9)?
        ));
    }
    let dest = device_of(host, call.arg(0)?)?;
    let (dest_x, dest_y) = (call.arg(1)? as i32, call.arg(2)? as i32);
    let (width, height) = (call.arg(3)? as i32, call.arg(4)? as i32);
    let source = device_of(host, call.arg(5)?)?;
    let (source_x, source_y) = (call.arg(6)? as i32, call.arg(7)? as i32);
    let (source_width, source_height) = (call.arg(8)? as i32, call.arg(9)? as i32);
    let key = call.arg(10)?;
    let key = (
        key as u8,
        ((key >> 8) & 0xff) as u8,
        ((key >> 16) & 0xff) as u8,
    );
    if width <= 0 || height <= 0 || source_width <= 0 || source_height <= 0 {
        return Ok(Return(1));
    }
    let mut painter = Painter::new(dest);
    for row in 0..height {
        for column in 0..width {
            let x = source_x + column * source_width / width;
            let y = source_y + row * source_height / height;
            let Some(colour) = gdi32::read_pixel(call.memory, &source, x, y)? else {
                continue;
            };
            if colour == key {
                continue;
            }
            painter.put(call.memory, dest_x + column, dest_y + row, colour)?;
        }
    }
    if let Some(surface) = painter.surface() {
        host.ddraw.touch(surface);
    }
    Ok(Return(1))
}
