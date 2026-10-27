//! `DDRAW.dll`: the surfaces the game draws into, and the palette that colours
//! them.
//!
//! The guest creates one `IDirectDraw`, a palette, a primary surface with a back
//! buffer, and draws into whatever `Lock` hands it. This module keeps that state:
//! each surface is a rectangle of guest memory the host allocated, with the pitch
//! the guest was told, so a `Lock` pointer stays valid until `Unlock` and the
//! guest's own addresses stay honest.
//!
//! # What "presentation" means here
//!
//! There is no display device in a page, so `Flip` is where the host copy of the
//! primary surface becomes the frame the page can draw: the back buffer is copied
//! onto the primary, the flip count is incremented, and the page reads the primary
//! buffer through the wasm exports when it wants a frame. Nothing is uploaded from
//! the shim itself.
//!
//! # Deliberate limits
//!
//! - 8 bits per pixel, 640x480, and one back buffer. The image asks for exactly
//!   that (`SetDisplayMode(640, 480, 8)`); anything else is refused rather than
//!   approximated, because a 16-bit frame would need the whole pixel-format path.
//! - `Blt`/`BltFast` copy rectangles without stretching or raster operations, but
//!   they do honour the colour keys: `SetColorKey` keeps the surface's source and
//!   destination key ranges, and a keyed blit leaves the pixels under the key
//!   alone. That is how the guest draws its logo and sprites over a background --
//!   without it every sprite carries its own rectangle of key colour. A stretched
//!   blit or an unimplemented raster op is refused with a fault so the log names
//!   it rather than drawing something wrong.
//! - `GetDC` hands out a GDI device context that draws straight into the
//!   surface's pixels, which is how a game copies a bitmap onto a surface. GDI
//!   text is still absent: a guest that reaches `TextOutA` stops, and the stop
//!   names it.

use std::collections::HashMap;

use crate::shim::{Action, Binding, Call, Host};
use crate::thunks::{self, m, Method};
use crate::Result;

use Action::Return;

const DDRAW: &str = "DDRAW.dll";

const DD_OK: u32 = 0;

/// `DDBLT_COLORFILL`: paint the destination rectangle with one palette index.
const DDBLT_COLORFILL: u32 = 0x0000_0400;

/// `DDBLT_KEYSRC` / `DDBLT_KEYDEST`: skip pixels that match the source surface's
/// source key, or the destination surface's destination key.
const DDBLT_KEYSRC: u32 = 0x0000_8000;
const DDBLT_KEYDEST: u32 = 0x0000_2000;

/// The same two ideas for `BltFast`, whose transfer flags are a separate set.
const DDBLTFAST_SRCCOLORKEY: u32 = 0x0000_0001;
const DDBLTFAST_DESTCOLORKEY: u32 = 0x0000_0002;

/// `SetColorKey`/`GetColorKey` flags: which of a surface's two keys is meant.
const DDCKEY_DESTBLT: u32 = 0x0000_0002;
const DDCKEY_SRCBLT: u32 = 0x0000_0008;
/// Offset of `dwFillColor` in the `DDBLTFX` the guest builds.
const FILL_COLOR: u32 = 0x50;
const E_NOINTERFACE: u32 = 0x8000_4002;
const DDERR_INVALIDPARAMS: u32 = 0x8007_0057;
/// `DDERR_INVALIDRECT`: the rectangle names no pixels at all.
const DDERR_INVALIDRECT: u32 = 0x8876_0096;

// DDSURFACEDESC offsets (DX5, 108 bytes).
const DESC_SIZE: u32 = 108;
const DESC_FLAGS: u32 = 4;
const DESC_HEIGHT: u32 = 8;
const DESC_WIDTH: u32 = 12;
const DESC_PITCH: u32 = 16;
const DESC_SURFACE: u32 = 36;
const DESC_CAPS: u32 = 104;

// `ddpfPixelFormat` inside the descriptor, and the fields of it that say what a
// pixel is. `dwRGBBitCount` is the mask a guest applies to a pixel it read back
// through a lock, which is how Cave Story turns a colour into one this display
// holds: it writes the colour into a pixel and masks what it reads. A zero bit
// count masks every converted colour down to black, which is the difference
// between a blue fade and a fade that never happens.
const DESC_PIXEL_FORMAT: u32 = 0x48;
const DESC_BITS: u32 = 0x54;
const DESC_RED_MASK: u32 = 0x58;
const DESC_GREEN_MASK: u32 = 0x5c;
const DESC_BLUE_MASK: u32 = 0x60;
const DDPIXELFORMAT_SIZE: u32 = 32;

// DDSD_* flags the guest sets.
const DDSD_CAPS: u32 = 0x1;
const DDSD_HEIGHT: u32 = 0x2;
const DDSD_WIDTH: u32 = 0x4;
const DDSD_PITCH: u32 = 0x8;
const DDSD_LPSURFACE: u32 = 0x100;
const DDSD_PIXELFORMAT: u32 = 0x1000;

// DDPF_*: how a pixel format describes its pixels.
const DDPF_PALETTEINDEXED8: u32 = 0x20;
const DDPF_RGB: u32 = 0x40;

// DDSCAPS_* flags this host understands.
const DDSCAPS_BACKBUFFER: u32 = 0x4;
const DDSCAPS_PRIMARYSURFACE: u32 = 0x200;
const DDSCAPS_OFFSCREENPLAIN: u32 = 0x40;
/// `DDSCAPS_COMPLEX` and `DDSCAPS_FLIP`: a primary surface that comes with its
/// back buffer attached, which is how the guest asks for one surface and then
/// queries the other.
const DDSCAPS_COMPLEX: u32 = 0x8;
const DDSCAPS_FLIP: u32 = 0x10;

/// The widest surface this host will allocate, so a bogus descriptor cannot eat
/// the arena.
const MAX_SURFACE_BYTES: u32 = 4 << 20;

/// One created surface.
struct Surface {
    width: u32,
    height: u32,
    pitch: u32,
    /// Bits per pixel: 8 for a palettised surface, 16 or 32 for truecolour. A
    /// blit moves `bits / 8` bytes per pixel, and a page needs the same number
    /// to read the pixels it is handed.
    bits: u8,
    /// Guest address of the pixels, `pitch * height` bytes the host allocated.
    buffer: u32,
    caps: u32,
    /// Locked rectangles: the guest may lock and unlock in a nested order, and the
    /// pointer only has to stay stable, which a bump-allocated buffer gives us.
    locks: u32,
    /// `DDCKEY_SRCBLT`: the colour values, low to high, that are transparent
    /// when this surface is the source of a keyed blit. The low 8, 16 or 24 bits
    /// are the value, matching [`Surface::bits`].
    src_key: Option<(u32, u32)>,
    /// `DDCKEY_DESTBLT`: destination pixels inside this range are left alone.
    dest_key: Option<(u32, u32)>,
    /// Bytes the host allocated for `buffer`, so releasing can hand them back.
    size: u32,
    /// COM reference count. The last `Release` frees the pixels.
    refs: u32,
}

/// `DDPCAPS_ALLOW256`: without it DirectDraw reserves the palette's first and
/// last entries, which stay black and white and refuse `SetEntries`. The guest
/// asks for `DDPCAPS_8BIT` alone, and its title palette carries a pink in entry
/// 0 (`255,145,145`, the artwork's colour key) that therefore never reaches the
/// screen. The version line is inked with index 0, so honouring the flag is the
/// difference between a black line and a pink one.
const DDPCAPS_ALLOW256: u32 = 0x40;

/// Everything `DirectDrawCreate` and the surfaces remember.
pub struct DirectDraw {
    width: u32,
    height: u32,
    /// Bits per pixel the port's screen uses, for the diagnostic notice.
    depth: u8,
    palette: [[u8; 3]; 256],
    /// Whether `CreatePalette` asked for `DDPCAPS_ALLOW256`.
    allow_256: bool,
    surfaces: HashMap<u32, Surface>,
    /// Scratch rows for a keyed blit, reused across rows and calls instead of
    /// allocated per row.
    row: Vec<u8>,
    composed: Vec<u8>,
    primary: Option<u32>,
    backbuffer: Option<u32>,
    cooperative_level: u32,
    flips: u64,
    /// Bumped every time the presented surface's pixels change without a flip.
    content_revision: u32,
    /// Bumped every time an entry's colour actually changes. A fade is a palette
    /// written frame after frame *without* a flip, so a page that redraws only on
    /// `flips` would show the one frame it caught and nothing after it: the
    /// publisher logo flashing past black is this counter's absence.
    palette_revision: u32,
    /// A surface the guest drew itself and the host was handed: `SDL_SetVideoMode`
    /// owns its pixels, so there is no `Surface` object and nothing to copy on a
    /// present. When set, this is what a page reads and how large it is.
    presented: Option<Presented>,
    /// The screen the host made for the window's context when the guest draws
    /// with GDI and has no DirectDraw display of its own. A menu built in a DIB
    /// and blitted through the window's context lands here, so it is what a page
    /// shows until the guest creates a display of its own.
    window_surface: Option<u32>,
}

/// The handle the host's own screen is recorded under. It is not a thunk object
/// and the guest never sees it, so it only has to be a number no surface the
/// guest creates can hold.
const WINDOW_SURFACE_HANDLE: u32 = 0x00ee_0000;

/// Pixels a non-DirectDraw library owns, in the shape a page reads them.
struct Presented {
    buffer: u32,
    width: u32,
    height: u32,
    pitch: u32,
    bits: u8,
}

impl Default for DirectDraw {
    fn default() -> Self {
        // The assumed mode, when the port has not named one: VGA 8-bit, which
        // any DirectDraw program of this era asks for anyway.
        Self::with_screen(640, 480, 8)
    }
}

impl DirectDraw {
    /// A display of the target's size. `depth` is carried for the notice: the
    /// surface this host builds is always palettised.
    pub fn with_screen(width: u32, height: u32, depth: u8) -> Self {
        Self {
            width,
            height,
            palette: [[0; 3]; 256],
            allow_256: false,
            surfaces: HashMap::new(),
            row: Vec::new(),
            composed: Vec::new(),
            primary: None,
            backbuffer: None,
            cooperative_level: 0,
            flips: 0,
            content_revision: 0,
            palette_revision: 0,
            presented: None,
            window_surface: None,
            depth,
        }
    }

    /// Gives the window's context a screen of its own.
    ///
    /// A program that draws its menus with GDI asks its window for a context and
    /// blits through it; if it never creates a DirectDraw primary surface, those
    /// pixels still have to land somewhere the page can show. This is that screen:
    /// a surface the size of the guest's window whose pixels the page reads
    /// exactly as it reads a primary surface's.
    pub fn make_window_surface(
        &mut self,
        buffer: u32,
        width: u32,
        height: u32,
        pitch: u32,
        size: u32,
        bits: u8,
    ) {
        self.surfaces.insert(
            WINDOW_SURFACE_HANDLE,
            Surface {
                width,
                height,
                pitch,
                bits,
                buffer,
                caps: 0,
                locks: 0,
                src_key: None,
                dest_key: None,
                size,
                refs: 1,
            },
        );
        self.window_surface = Some(WINDOW_SURFACE_HANDLE);
    }

    /// The screen the window's context draws into while the guest has no display
    /// of its own.
    pub fn window_surface(&self) -> Option<u32> {
        self.window_surface
    }

    /// Gives the window's screen up, when the guest creates a display: from then
    /// on the primary surface is what the page shows, and two screens would be
    /// one too many.
    pub fn retire_window_surface(&mut self) -> Option<u32> {
        let handle = self.window_surface.take()?;
        self.surfaces.remove(&handle).map(|surface| surface.buffer)
    }

    /// Adopts a buffer the guest allocated for itself (SDL's video surface) as
    /// what a page presents. The host does not own it and never copies it: the
    /// guest's own writes are the frame.
    pub fn publish(&mut self, buffer: u32, width: u32, height: u32, pitch: u32, bits: u8) {
        self.presented = Some(Presented {
            buffer,
            width,
            height,
            pitch,
            bits,
        });
    }

    /// Forgets an adopted buffer, when the guest gives its video mode up.
    pub fn unpublish(&mut self) {
        self.presented = None;
    }

    /// A frame the guest presented: the counter a page watches for change.
    pub fn present(&mut self) {
        self.flips += 1;
    }

    /// Bits per pixel of what is presented: 8 for a palettised surface, 16 or 32
    /// for a truecolour one. A page needs this to read the pixels correctly.
    pub fn bits(&self) -> u8 {
        if let Some(presented) = self.presented.as_ref() {
            return presented.bits;
        }
        // The window's screen stands in for a primary surface while the guest is
        // drawing with GDI, so a page reads it with the same depth.
        self.primary
            .or(self.window_surface)
            .and_then(|primary| self.surfaces.get(&primary))
            .map_or(self.depth, |surface| surface.bits)
    }
}

impl DirectDraw {
    pub fn width(&self) -> u32 {
        if let Some(presented) = self.presented.as_ref() {
            return presented.width;
        }
        self.primary
            .or(self.window_surface)
            .and_then(|handle| self.surfaces.get(&handle))
            .map_or(self.width, |surface| surface.width)
    }

    pub fn height(&self) -> u32 {
        if let Some(presented) = self.presented.as_ref() {
            return presented.height;
        }
        self.primary
            .or(self.window_surface)
            .and_then(|handle| self.surfaces.get(&handle))
            .map_or(self.height, |surface| surface.height)
    }

    /// The 256-entry palette the guest set, RGB triples in index order.
    pub fn palette(&self) -> &[[u8; 3]; 256] {
        &self.palette
    }

    /// Writes one entry, unless DirectDraw reserves it: indices 0 and 255 are
    /// unavailable to a palette created without `DDPCAPS_ALLOW256`.
    fn set_entry(&mut self, index: usize, colour: [u8; 3]) {
        if !self.allow_256 && (index == 0 || index == 255) {
            return;
        }
        if let Some(slot) = self.palette.get_mut(index) {
            if *slot != colour {
                *slot = colour;
                self.palette_revision = self.palette_revision.wrapping_add(1);
            }
        }
    }

    /// The reserved pair's fixed colours: index 0 black, index 255 white.
    fn reserve_ends(&mut self) {
        if self.allow_256 {
            return;
        }
        // Written directly: `set_entry` refuses these two by design.
        for (index, colour) in [(0, [0, 0, 0]), (255, [255, 255, 255])] {
            if self.palette[index] != colour {
                self.palette[index] = colour;
                self.palette_revision = self.palette_revision.wrapping_add(1);
            }
        }
    }

    pub fn flips(&self) -> u64 {
        self.flips
    }

    /// How many times the palette has changed, which is the other half of "has
    /// the picture changed": a fade writes entries without presenting a frame.
    /// Bumped when the presented surface's pixels change without a `Flip`.
    ///
    /// A game that blits straight onto the primary surface -- Cave Story draws its
    /// credits that way -- never touches the flip counter, so a page that watched
    /// only `flips` would show the first frame forever.
    pub fn content_revision(&self) -> u32 {
        self.content_revision
    }

    /// Notes that a surface was written to, and counts it if it is the one the
    /// page shows.
    pub fn touch(&mut self, surface: u32) {
        if self.primary == Some(surface) || self.window_surface == Some(surface) {
            self.content_revision = self.content_revision.wrapping_add(1);
        }
    }

    pub fn palette_revision(&self) -> u32 {
        self.palette_revision
    }

    /// The pixels a page presents: the adopted buffer when a library owns them,
    /// otherwise the primary surface `Flip` draws onto.
    pub fn framebuffer(&self) -> Option<(u32, u32, u32)> {
        if let Some(presented) = self.presented.as_ref() {
            return Some((presented.buffer, presented.width, presented.height));
        }
        let surface = self.surfaces.get(&self.primary.or(self.window_surface)?)?;
        Some((surface.buffer, surface.width, surface.height))
    }

    /// Bytes per row of the primary surface, which is its width for a
    /// linear 8-bit surface but is read from the surface rather than assumed.
    pub fn framebuffer_pitch(&self) -> u32 {
        if let Some(presented) = self.presented.as_ref() {
            return presented.pitch;
        }
        self.primary
            .or(self.window_surface)
            .and_then(|primary| self.surfaces.get(&primary))
            .map_or(0, |surface| surface.pitch)
    }

    pub fn surface_count(&self) -> usize {
        self.surfaces.len()
    }

    /// The surface `Flip` presents onto, and the one it copies from.
    ///
    /// A page needs both to say "this is what you are looking at": the primary
    /// can be blank while the guest has drawn into the back buffer, and a host
    /// that only reports the flip count cannot tell that apart from a stall.
    pub fn primary_surface(&self) -> Option<u32> {
        self.primary
    }

    pub fn back_surface(&self) -> Option<u32> {
        self.backbuffer
    }

    /// Pixels of the presented surface, or of the surface a `Flip` copies from.
    /// These are guest addresses: what a page reads to show the guest's screen.
    pub fn primary_buffer(&self) -> Option<u32> {
        self.primary
            .or(self.window_surface)
            .and_then(|handle| self.surfaces.get(&handle))
            .map(|surface| surface.buffer)
    }

    pub fn back_buffer(&self) -> Option<u32> {
        self.backbuffer
            .and_then(|handle| self.surfaces.get(&handle))
            .map(|surface| surface.buffer)
    }

    /// A surface's pixels and shape: `buffer`, `width`, `height`, `pitch` and
    /// bits per pixel. A device context blits into one of these, so it asks the
    /// surface rather than keeping a second copy of the geometry.
    pub fn surface(&self, handle: u32) -> Option<(u32, u32, u32, u32, u8)> {
        self.surfaces.get(&handle).map(|surface| {
            (
                surface.buffer,
                surface.width,
                surface.height,
                surface.pitch,
                surface.bits,
            )
        })
    }

    /// Every surface's buffer and how often the guest locked it, in creation
    /// order is not defined, so callers match on the buffer address instead.
    pub fn surface_buffers(&self) -> Vec<(u32, u32, u32, u32)> {
        self.surfaces
            .values()
            .map(|surface| (surface.buffer, surface.width, surface.height, surface.locks))
            .collect()
    }
}

/// What `CreateSurface` was asked for.
struct SurfaceDesc {
    width: u32,
    height: u32,
    pitch: u32,
    bits: u8,
    caps: u32,
}

/// Reads a `DDSURFACEDESC` the guest filled in.
///
/// `bits` is the display's depth, which decides the pitch when the guest leaves
/// `lPitch` at zero -- the usual case, because DirectDraw computes it from the
/// width and the pixel format.
fn read_desc(
    call: &Call<'_>,
    address: u32,
    default: (u32, u32),
    bits: u8,
) -> Result<SurfaceDesc> {
    if address == 0 {
        return Err("DirectDraw called without a surface descriptor".to_string());
    }
    let flags = call.memory.read_u32(address + DESC_FLAGS)?;
    let height = call.memory.read_u32(address + DESC_HEIGHT)?;
    let width = call.memory.read_u32(address + DESC_WIDTH)?;
    let pitch = call.memory.read_u32(address + DESC_PITCH)?;
    let caps = call.memory.read_u32(address + DESC_CAPS)?;
    let (width, height) = if flags & (DDSD_WIDTH | DDSD_HEIGHT) == (DDSD_WIDTH | DDSD_HEIGHT) {
        (width, height)
    } else {
        default
    };
    Ok(SurfaceDesc {
        width,
        height,
        pitch: if pitch == 0 {
            width * u32::from(bits / 8)
        } else {
            pitch
        },
        bits,
        caps,
    })
}

fn write_desc(desc: &mut Call<'_>, address: u32, surface: &Surface) -> Result<()> {
    desc.zero(address, DESC_SIZE)?;
    desc.write_u32(address, DESC_SIZE)?;
    desc.write_u32(
        address + DESC_FLAGS,
        DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PITCH | DDSD_LPSURFACE | DDSD_PIXELFORMAT,
    )?;
    desc.write_u32(address + DESC_HEIGHT, surface.height)?;
    desc.write_u32(address + DESC_WIDTH, surface.width)?;
    desc.write_u32(address + DESC_PITCH, surface.pitch)?;
    desc.write_u32(address + DESC_SURFACE, surface.buffer)?;
    desc.write_u32(address + DESC_CAPS, surface.caps)?;
    write_pixel_format(desc, address, surface.bits)?;
    Ok(())
}

/// Fills in the descriptor's `ddpfPixelFormat` for a surface of `bits` per
/// pixel, the masks included: a guest that asks what a surface's pixels look
/// like gets the layout this host stores them in, not a blank one.
fn write_pixel_format(desc: &mut Call<'_>, address: u32, bits: u8) -> Result<()> {
    let (flags, red, green, blue) = match bits {
        8 => (DDPF_PALETTEINDEXED8, 0, 0, 0),
        16 => (DDPF_RGB, 0xf800, 0x07e0, 0x001f),
        _ => (DDPF_RGB, 0x00ff_0000, 0x0000_ff00, 0x0000_00ff),
    };
    let format = address + DESC_PIXEL_FORMAT;
    desc.write_u32(format, DDPIXELFORMAT_SIZE)?;
    desc.write_u32(format + 4, flags)?;
    desc.write_u32(address + DESC_BITS, u32::from(bits))?;
    desc.write_u32(address + DESC_RED_MASK, red)?;
    desc.write_u32(address + DESC_GREEN_MASK, green)?;
    desc.write_u32(address + DESC_BLUE_MASK, blue)?;
    Ok(())
}

pub const SHIMS: &[Binding] = &[
    Binding { dll: DDRAW, name: "DirectDrawCreate", arg_bytes: 12, shim: direct_draw_create },
];

fn direct_draw_create(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(1)?;
    let outer = call.arg(2)?;
    if outer != 0 {
        return Ok(Return(E_NOINTERFACE));
    }
    if destination == 0 {
        return Ok(Return(DDERR_INVALIDPARAMS));
    }
    let object = thunks::new_object(call.memory, host, thunks::DDRAW)?;
    call.write_u32(destination, object)?;
    host.notice(format!(
        "DirectDraw: object created ({}x{}x{}, palette and one back buffer)",
        host.ddraw.width, host.ddraw.height, host.ddraw.depth
    ));
    Ok(Return(DD_OK))
}

/// `IDirectDrawSurface::AddRef`: one more reference, which is returned.
pub(crate) fn add_ref(host: &mut Host, this: u32) -> u32 {
    let Some(surface) = host.ddraw.surfaces.get_mut(&this) else {
        return 0;
    };
    surface.refs = surface.refs.saturating_add(1);
    surface.refs
}

/// `IDirectDrawSurface::Release`: one reference fewer, and at zero the surface
/// is gone and its pixels go back to the allocator.
///
/// This is what keeps a board session possible. The guest creates a surface per
/// tile as it loads the map and releases it again; when Release was a no-op every
/// one of them kept its pixels for the life of the page, and the arena -- which
/// every other host allocation shares -- ran out with the board still blank.
pub(crate) fn release(host: &mut Host, this: u32) -> u32 {
    if let Some(surface) = host.ddraw.surfaces.get_mut(&this) {
        if surface.refs > 1 {
            surface.refs -= 1;
            return surface.refs;
        }
    }
    let Some(surface) = host.ddraw.surfaces.remove(&this) else {
        return 0;
    };
    host.release(surface.buffer);
    if host.ddraw.primary == Some(this) {
        host.ddraw.primary = None;
    }
    if host.ddraw.backbuffer == Some(this) {
        host.ddraw.backbuffer = None;
    }
    0
}

pub(crate) const DRAW_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("Compact", 4),
    m("CreateClipper", 16),
    m("CreatePalette", 20),
    m("CreateSurface", 16),
    m("DuplicateSurface", 12),
    m("EnumDisplayModes", 20),
    m("EnumSurfaces", 20),
    m("FlipToGDISurface", 4),
    m("GetCaps", 12),
    m("GetDisplayMode", 8),
    m("GetFourCCCodes", 12),
    m("GetGDISurface", 8),
    m("GetMonitorFrequency", 8),
    m("GetScanLine", 8),
    m("GetVerticalBlankStatus", 8),
    m("Initialize", 8),
    m("RestoreDisplayMode", 4),
    m("SetCooperativeLevel", 12),
    m("SetDisplayMode", 16),
    m("WaitForVerticalBlank", 12),
];

pub(crate) const SURFACE_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("AddAttachedSurface", 8),
    m("AddOverlayDirtyRect", 8),
    m("Blt", 24),
    m("BltBatch", 20),
    m("BltFast", 24),
    m("DeleteAttachedSurface", 12),
    m("EnumAttachedSurfaces", 12),
    m("EnumOverlayZOrders", 16),
    m("Flip", 12),
    m("GetAttachedSurface", 12),
    m("GetBltStatus", 8),
    m("GetCaps", 8),
    m("GetClipper", 8),
    m("GetColorKey", 12),
    m("GetDC", 8),
    m("GetFlipStatus", 8),
    m("GetOverlayPosition", 12),
    m("GetPalette", 8),
    m("GetPixelFormat", 8),
    m("GetSurfaceDesc", 8),
    m("Initialize", 12),
    m("IsLost", 4),
    m("Lock", 20),
    m("ReleaseDC", 8),
    m("Restore", 4),
    m("SetClipper", 8),
    m("SetColorKey", 12),
    m("SetOverlayPosition", 12),
    m("SetPalette", 8),
    m("Unlock", 8),
    m("UpdateOverlay", 16),
    m("UpdateOverlayDisplay", 8),
    m("UpdateOverlayZOrder", 12),
];

pub(crate) const PALETTE_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("GetCaps", 12),
    m("GetEntries", 20),
    m("Initialize", 16),
    m("SetEntries", 20),
];

pub(crate) const CLIPPER_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("GetClipList", 16),
    m("GetHWnd", 8),
    m("Initialize", 12),
    m("IsClipListChanged", 8),
    m("SetClipList", 12),
    m("SetHWnd", 12),
];

pub(crate) fn serve(
    host: &mut Host,
    call: &mut Call<'_>,
    kind: u8,
    name: &'static str,
) -> Result<Action> {
    if std::env::var("SHIMKIT_LOG_DDRAW").is_ok() && host.notices.len() < 600 {
        host.notice(format!("ddraw {name}"));
    }
    // `SHIMKIT_TRACE_SURFACE=<handle>` follows one surface: every method the
    // guest calls on it, which is how a surface that comes out black is traced
    // to whether the guest ever wrote to it at all.
    if let Ok(wanted) = std::env::var("SHIMKIT_TRACE_SURFACE") {
        if let Some(handle) = wanted.trim().strip_prefix("0x").and_then(|hex| u32::from_str_radix(hex, 16).ok()) {
            if kind == thunks::DDRAW_SURFACE && call.arg(0).ok() == Some(handle) {
                println!(
                    "surface {name} args={:08x},{:08x},{:08x},{:08x}",
                    call.arg(1).unwrap_or(0),
                    call.arg(2).unwrap_or(0),
                    call.arg(3).unwrap_or(0),
                    call.arg(4).unwrap_or(0),
                );
            }
        }
    }
    match (kind, name) {
        // A surface is reference counted for real: the last `Release` frees its
        // pixels, which is what keeps a board session inside the heap. Every
        // other DirectDraw object outlives its references here.
        (thunks::DDRAW_SURFACE, "AddRef") => Ok(Return(add_ref(host, call.arg(0)?))),
        (thunks::DDRAW_SURFACE, "Release") => Ok(Return(release(host, call.arg(0)?))),
        (_, "AddRef") | (_, "Release") => Ok(Return(1)),
        (_, "QueryInterface") => Ok(Return(E_NOINTERFACE)),

        // ---- IDirectDraw --------------------------------------------------
        (thunks::DDRAW, "Compact") | (thunks::DDRAW, "FlipToGDISurface") => Ok(Return(DD_OK)),
        (thunks::DDRAW, "SetCooperativeLevel") => {
            host.ddraw.cooperative_level = call.arg(2)?;
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "SetDisplayMode") => {
            let width = call.arg(1)?;
            let height = call.arg(2)?;
            let depth = call.arg(3)?;
            // 8 is what a palettised game asks for; 16 and 32 are what a game
            // with its own artwork asks for, and a surface of that depth is
            // bytes the page reads directly. 15 and 24 are modes whose pixels
            // are not laid out the way a page can read them.
            let Ok(depth) = u8::try_from(depth) else {
                return Err(format!("DirectDraw asked for {depth} bits per pixel"));
            };
            if !matches!(depth, 8 | 16 | 32) {
                return Err(format!(
                    "DirectDraw asked for {depth} bits per pixel; this host presents 8, 16 or 32"
                ));
            }
            host.ddraw.width = width;
            host.ddraw.height = height;
            host.ddraw.depth = depth;
            host.notice(format!("DirectDraw: display mode {width}x{height}x{depth}"));
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "RestoreDisplayMode") | (thunks::DDRAW, "WaitForVerticalBlank") => {
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "GetCaps") => {
            // DDCAPS is a large structure; zeroes mean "no hardware features",
            // which is what an 8-bit software surface has.
            let caps = call.arg(1)?;
            if caps != 0 {
                call.zero(caps, 380)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "CreateClipper") => {
            let destination = call.arg(2)?;
            let clipper = thunks::new_object(call.memory, host, thunks::DDRAW_CLIPPER)?;
            call.write_u32(destination, clipper)?;
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "CreatePalette") => {
            let flags = call.arg(1)?;
            let entries = call.arg(2)?;
            let destination = call.arg(3)?;
            host.ddraw.allow_256 = flags & DDPCAPS_ALLOW256 != 0;
            if entries != 0 {
                let bytes = call.memory.read_bytes(entries, 256 * 4)?.to_vec();
                for index in 0..256 {
                    host.ddraw
                        .set_entry(index, [bytes[index * 4], bytes[index * 4 + 1], bytes[index * 4 + 2]]);
                }
            }
            // The reserved pair is black and white whatever the array holds.
            host.ddraw.reserve_ends();
            let palette = thunks::new_object(call.memory, host, thunks::DDRAW_PALETTE)?;
            call.write_u32(destination, palette)?;
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "CreateSurface") => {
            let depth = host.ddraw.depth;
            let desc = read_desc(
                call,
                call.arg(1)?,
                (host.ddraw.width, host.ddraw.height),
                depth,
            )?;
            if desc.caps & (DDSCAPS_PRIMARYSURFACE | DDSCAPS_OFFSCREENPLAIN | DDSCAPS_BACKBUFFER)
                == 0
            {
                return Err(format!(
                    "DirectDraw surface {}x{} asks for no capability this host models ({:#x})",
                    desc.width, desc.height, desc.caps
                ));
            }
            let size = desc.pitch.checked_mul(desc.height).unwrap_or(u32::MAX);
            if size > MAX_SURFACE_BYTES {
                return Err(format!(
                    "DirectDraw surface {}x{} at pitch {} is larger than this host allocates",
                    desc.width, desc.height, desc.pitch
                ));
            }
            let buffer = host.allocate(size, 4)?;
            let surface = Surface {
                width: desc.width,
                height: desc.height,
                pitch: desc.pitch,
                bits: desc.bits,
                buffer,
                caps: desc.caps,
                locks: 0,
                src_key: None,
                dest_key: None,
                size,
                refs: 1,
            };
            let is_primary = desc.caps & DDSCAPS_PRIMARYSURFACE != 0;
            let is_back = desc.caps & DDSCAPS_BACKBUFFER != 0;
            let complex = desc.caps & (DDSCAPS_COMPLEX | DDSCAPS_FLIP) != 0;
            let object = thunks::new_object(call.memory, host, thunks::DDRAW_SURFACE)?;
            host.ddraw.surfaces.insert(object, surface);
            if is_primary {
                // The guest has a display now, so the screen the window's context
                // was given is no longer what a page should show.
                if let Some(buffer) = host.ddraw.retire_window_surface() {
                    host.release(buffer);
                }
                host.ddraw.primary = Some(object);
                if complex && host.ddraw.backbuffer.is_none() {
                    // A complex or flipping primary owns one back buffer, which the
                    // guest reaches with `GetAttachedSurface` rather than by
                    // creating it: DirectDraw allocated it with the primary.
                    let attached = Surface {
                        width: desc.width,
                        height: desc.height,
                        pitch: desc.pitch,
                        bits: desc.bits,
                        buffer: host.allocate(size, 4)?,
                        caps: DDSCAPS_BACKBUFFER,
                        locks: 0,
                        src_key: None,
                        dest_key: None,
                        size,
                        refs: 1,
                    };
                    let back = thunks::new_object(call.memory, host, thunks::DDRAW_SURFACE)?;
                    host.ddraw.surfaces.insert(back, attached);
                    host.ddraw.backbuffer = Some(back);
                }
            } else if is_back || host.ddraw.backbuffer.is_none() {
                host.ddraw.backbuffer = Some(object);
            }
            let destination = call.arg(2)?;
            call.write_u32(destination, object)?;
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "GetGDISurface") => {
            // The surface GDI draws on is the primary one here: a fullscreen
            // window with no GDI-owned buffer of its own.
            let destination = call.arg(1)?;
            if destination != 0 {
                call.write_u32(destination, host.ddraw.primary.unwrap_or(0))?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "GetDisplayMode") => {
            let destination = call.arg(1)?;
            if destination != 0 {
                let surface = Surface {
                    width: host.ddraw.width,
                    height: host.ddraw.height,
                    pitch: host.ddraw.width * u32::from(host.ddraw.depth / 8),
                    bits: host.ddraw.depth,
                    buffer: 0,
                    caps: 0,
                    locks: 0,
                    src_key: None,
                    dest_key: None,
                    size: 0,
                    refs: 1,
                };
                write_desc(call, destination, &surface)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW, "GetMonitorFrequency")
        | (thunks::DDRAW, "GetScanLine") | (thunks::DDRAW, "GetVerticalBlankStatus")
        | (thunks::DDRAW, "GetFourCCCodes") | (thunks::DDRAW, "Initialize")
        | (thunks::DDRAW, "DuplicateSurface") | (thunks::DDRAW, "EnumDisplayModes")
        | (thunks::DDRAW, "EnumSurfaces") => Ok(Return(DD_OK)),

        // ---- IDirectDrawSurface ------------------------------------------
        (thunks::DDRAW_SURFACE, "IsLost") => Ok(Return(DD_OK)),
        (thunks::DDRAW_SURFACE, "Restore") => Ok(Return(DD_OK)),
        (thunks::DDRAW_SURFACE, "GetBltStatus") | (thunks::DDRAW_SURFACE, "GetFlipStatus") => {
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "Lock") => lock(host, call),
        (thunks::DDRAW_SURFACE, "Unlock") => {
            let this = call.arg(0)?;
            if let Some(surface) = surface_of(host, call) {
                if surface.locks > 0 {
                    surface.locks -= 1;
                }
            }
            // Pixels written through the locked pointer are on screen now.
            host.ddraw.touch(this);
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "Flip") => {
            flip(host, call)?;
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "GetSurfaceDesc") => {
            let this = call.arg(0)?;
            let destination = call.arg(1)?;
            if let Some(surface) = host.ddraw.surfaces.get(&this) {
                let surface = Surface {
                    width: surface.width,
                    height: surface.height,
                    pitch: surface.pitch,
                    bits: surface.bits,
                    buffer: surface.buffer,
                    caps: surface.caps,
                    locks: 0,
                    src_key: surface.src_key,
                    dest_key: surface.dest_key,
                    size: surface.size,
                    refs: 1,
                };
                write_desc(call, destination, &surface)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "GetCaps") => {
            let this = call.arg(0)?;
            let destination = call.arg(1)?;
            if destination != 0 {
                let caps = host
                    .ddraw
                    .surfaces
                    .get(&this)
                    .map_or(0, |surface| surface.caps);
                call.write_u32(destination, caps)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "GetAttachedSurface") => {
            let requested = call.arg(1)?;
            let destination = call.arg(2)?;
            // `GetAttachedSurface` is given a *pointer* to a `DDSCAPS` naming what
            // to find, not the flags themselves.
            let requested = call.memory.read_u32(requested)?;
            // The only attachment a fullscreen flipping primary has is its back
            // buffer; asking for anything else gets a null like DirectDraw does.
            let attached = if requested & DDSCAPS_BACKBUFFER != 0 {
                host.ddraw.backbuffer.unwrap_or(0)
            } else {
                0
            };
            if destination != 0 {
                call.write_u32(destination, attached)?;
            }
            Ok(Return(DD_OK))
        }
        // The palette is the one the guest set through `IDirectDrawPalette`, and
        // the only clip is the surface itself: a fullscreen 640x480 window has
        // nothing to clip against.
        (thunks::DDRAW_SURFACE, "SetPalette") | (thunks::DDRAW_SURFACE, "SetClipper") => {
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "GetClipper") => {
            let destination = call.arg(1)?;
            if destination != 0 {
                call.write_u32(destination, 0)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_SURFACE, "SetColorKey") => set_color_key(host, call),
        (thunks::DDRAW_SURFACE, "GetColorKey") => get_color_key(host, call),
        (thunks::DDRAW_SURFACE, "AddAttachedSurface")
        | (thunks::DDRAW_SURFACE, "AddOverlayDirtyRect")
        | (thunks::DDRAW_SURFACE, "EnumAttachedSurfaces")
        | (thunks::DDRAW_SURFACE, "EnumOverlayZOrders")
        | (thunks::DDRAW_SURFACE, "Initialize")
        | (thunks::DDRAW_SURFACE, "SetOverlayPosition")
        | (thunks::DDRAW_SURFACE, "GetOverlayPosition")
        | (thunks::DDRAW_SURFACE, "GetPixelFormat")
        | (thunks::DDRAW_SURFACE, "UpdateOverlay")
        | (thunks::DDRAW_SURFACE, "UpdateOverlayDisplay")
        | (thunks::DDRAW_SURFACE, "UpdateOverlayZOrder") => Ok(Return(DD_OK)),
        (thunks::DDRAW_SURFACE, "Blt") | (thunks::DDRAW_SURFACE, "BltFast") => {
            let this = call.arg(0)?;
            let action = blit(host, call, name)?;
            // A blit onto the surface the page shows is a presented frame, even
            // though no `Flip` counted it.
            host.ddraw.touch(this);
            Ok(action)
        }
        (thunks::DDRAW_SURFACE, "GetDC") => {
            // GDI draws straight into the surface's pixels: the DC a guest gets
            // back is the one `StretchBlt` and `BitBlt` write through. Like the
            // rest of the interface, the answer comes through `lphdc` and the
            // return value is the `HRESULT`.
            let surface = call.arg(0)?;
            let out = call.arg(1)?;
            let dc = host.gdi.dc_for_surface(surface);
            if out != 0 {
                call.write_u32(out, dc)?;
            }
            Ok(Return(0))
        }
        (thunks::DDRAW_SURFACE, "ReleaseDC") => Ok(Return(0)),

        // ---- IDirectDrawPalette ------------------------------------------
        (thunks::DDRAW_PALETTE, "SetEntries") => {
            let start = call.arg(2)? as usize;
            let count = call.arg(3)? as usize;
            let entries = call.arg(4)?;
            if entries != 0 && start < 256 {
                let count = count.min(256 - start);
                let bytes = call.memory.read_bytes(entries, count * 4)?.to_vec();
                for index in 0..count {
                    host.ddraw.set_entry(
                        start + index,
                        [
                            bytes[index * 4],
                            bytes[index * 4 + 1],
                            bytes[index * 4 + 2],
                        ],
                    );
                }
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_PALETTE, "GetEntries") => Ok(Return(DD_OK)),
        (thunks::DDRAW_PALETTE, "GetCaps") => {
            let caps = call.arg(2)?;
            if caps != 0 {
                call.write_u32(caps, 0)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_PALETTE, "Initialize") => Ok(Return(DD_OK)),

        // ---- IDirectDrawClipper ------------------------------------------
        (thunks::DDRAW_CLIPPER, "GetHWnd") => {
            let destination = call.arg(1)?;
            if destination != 0 {
                call.write_u32(destination, 0)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_CLIPPER, "SetHWnd")
        | (thunks::DDRAW_CLIPPER, "SetClipList")
        | (thunks::DDRAW_CLIPPER, "GetClipList") => Ok(Return(DD_OK)),
        (thunks::DDRAW_CLIPPER, "IsClipListChanged") => {
            let changed = call.arg(1)?;
            if changed != 0 {
                call.write_u32(changed, 0)?;
            }
            Ok(Return(DD_OK))
        }
        (thunks::DDRAW_CLIPPER, "Initialize") => Ok(Return(DD_OK)),

        (kind, name) => Err(format!(
            "{}::{name} is not modelled",
            thunks::kind_name(kind)
        )),
    }
}

fn surface_of<'a>(host: &'a mut Host, call: &Call<'_>) -> Option<&'a mut Surface> {
    let this = call.arg(0).ok()?;
    host.ddraw.surfaces.get_mut(&this)
}

/// Reads a `RECT` and keeps it inside the surface. A null rectangle means the
/// whole surface, which is what every optional-rectangle API means.
fn bounds(call: &Call<'_>, rect: u32, surface: &Surface) -> Result<(i32, i32, i32, i32)> {
    let (left, top, right, bottom) = if rect == 0 {
        (0, 0, surface.width as i32, surface.height as i32)
    } else {
        (
            call.memory.read_u32(rect)? as i32,
            call.memory.read_u32(rect + 4)? as i32,
            call.memory.read_u32(rect + 8)? as i32,
            call.memory.read_u32(rect + 12)? as i32,
        )
    };
    if left < 0 || top < 0 || right > surface.width as i32 || bottom > surface.height as i32 {
        return Err(format!(
            "DirectDraw: rectangle {left},{top}..{right},{bottom} is outside a {}x{} surface",
            surface.width, surface.height
        ));
    }
    Ok((left, top, right, bottom))
}

fn lock(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let rect = call.arg(1)?;
    let destination = call.arg(2)?;
    let Some(surface) = host.ddraw.surfaces.get(&this) else {
        return Err(format!("DirectDraw: Lock on {this:#010x}, which is not a surface"));
    };
    let (left, top, right, bottom) = bounds(call, rect, surface)?;
    let width = (right - left) as u32;
    let height = (bottom - top) as u32;
    // Columns are pixels, so the byte offset is the column times the pixel size:
    // a 16-bit lock that added the column raw would hand back a pointer half a
    // pixel per column off.
    let bytes = u32::from(surface.bits / 8).max(1);
    let offset = top as u32 * surface.pitch + left as u32 * bytes;
    let pointer = surface.buffer + offset;
    let pitch = surface.pitch;
    let bits = surface.bits;
    // A surface the guest fills itself -- a background it renders in code rather
    // than loading -- is only ever written through a lock, so a region that
    // comes out black is traced to whether the lock happened at all.
    let line = format!(
        "lock {this:#010x} {}x{}x{} rect {left},{top},{right},{bottom} pitch {pitch} at {pointer:#010x}",
        surface.width, surface.height, surface.bits
    );
    if let Some(surface) = host.ddraw.surfaces.get_mut(&this) {
        surface.locks += 1;
    }
    trace(host, line);
    call.zero(destination, DESC_SIZE)?;
    call.write_u32(destination, DESC_SIZE)?;
    call.write_u32(
        destination + DESC_FLAGS,
        DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PITCH | DDSD_LPSURFACE | DDSD_PIXELFORMAT,
    )?;
    call.write_u32(destination + DESC_HEIGHT, height)?;
    call.write_u32(destination + DESC_WIDTH, width)?;
    call.write_u32(destination + DESC_PITCH, pitch)?;
    call.write_u32(destination + DESC_SURFACE, pointer)?;
    write_pixel_format(call, destination, bits)?;
    Ok(Return(DD_OK))
}


fn flip(host: &mut Host, call: &mut Call<'_>) -> Result<()> {
    let Some(back) = host.ddraw.backbuffer else {
        // A single surface: the guest is already drawing where the page looks.
        host.ddraw.flips += 1;
        return Ok(());
    };
    let Some(source) = host.ddraw.surfaces.get(&back) else {
        host.ddraw.flips += 1;
        return Ok(());
    };
    let Some(primary) = host.ddraw.primary.and_then(|p| host.ddraw.surfaces.get(&p)) else {
        host.ddraw.flips += 1;
        return Ok(());
    };
    // The two buffers were created by the guest with the same geometry; a mismatch
    // is the guest's own doing and is reported rather than clipped.
    if source.width != primary.width || source.height != primary.height {
        return Err(format!(
            "DirectDraw: Flip from {}x{} onto {}x{}",
            source.width, source.height, primary.width, primary.height
        ));
    }
    let rows = source.height as usize;
    let row_bytes = source.width as usize * usize::from(source.bits / 8);
    let from = source.buffer;
    let to = primary.buffer;
    let from_pitch = source.pitch;
    let to_pitch = primary.pitch;
    // One copy per row inside guest memory: this runs on every presented frame,
    // and a buffer per row was an allocation per row.
    for row in 0..rows {
        let at = row as u32;
        call.memory
            .copy(from + at * from_pitch, to + at * to_pitch, row_bytes)?;
    }
    host.ddraw.flips += 1;
    Ok(())
}

/// `IDirectDrawSurface::SetColorKey(this, flags, lpDDColorKey)`.
///
/// A `DDCOLORKEY` is two dwords, low then high; on an 8-bit surface both are
/// palette indices, and the range between them is the transparent one. The guest
/// uses a source key on its page-sized and sprite-sized surfaces and never asks
/// for it back, but a host that drops the key paints every sprite's background
/// over what was behind it, so the key has to be kept.
fn set_color_key(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let flags = call.arg(1)?;
    let key = call.arg(2)?;
    let range = if key == 0 {
        None
    } else {
        let low = call.memory.read_u32(key)?;
        let high = call.memory.read_u32(key + 4)?;
        Some((low.min(high), low.max(high)))
    };
    if let Some(surface) = host.ddraw.surfaces.get_mut(&this) {
        if flags & DDCKEY_SRCBLT != 0 {
            surface.src_key = range;
        }
        if flags & DDCKEY_DESTBLT != 0 {
            surface.dest_key = range;
        }
    }
    Ok(Return(DD_OK))
}

/// `IDirectDrawSurface::GetColorKey(this, flags, lpDDColorKey)`, so a guest that
/// asks what it set gets the same range back.
fn get_color_key(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let flags = call.arg(1)?;
    let destination = call.arg(2)?;
    if destination == 0 {
        return Ok(Return(DD_OK));
    }
    let surface = host.ddraw.surfaces.get(&this);
    let range = surface.and_then(|surface| {
        if flags & DDCKEY_DESTBLT != 0 {
            surface.dest_key
        } else {
            surface.src_key
        }
    });
    let (low, high) = range.unwrap_or((0, 0));
    call.write_u32(destination, low)?;
    call.write_u32(destination + 4, high)?;
    Ok(Return(DD_OK))
}

/// A rectangle copy. No stretching: the guest only ever copies same-format
/// surfaces at the same size. A blit may be keyed, though -- the guest masks its
/// sprites that way -- so the key ranges are honoured pixel by pixel.
fn blit(host: &mut Host, call: &mut Call<'_>, method: &'static str) -> Result<Action> {
    let this = call.arg(0)?;
    // `BltFast(dx, dy, lpDDSrcSurface, lpSrcRect, dwTrans)` positions the copy
    // with two integers; `Blt(lpDestRect, lpDDSrcSurface, lpSrcRect, dwFlags,
    // lpDDBltFx)` puts it in a rectangle. Both carry a source rectangle.
    let (rect, source, source_rect, dx, dy) = if method == "BltFast" {
        (
            0,
            call.arg(3)?,
            call.arg(4)?,
            call.arg(1)? as i32,
            call.arg(2)? as i32,
        )
    } else {
        let rect = call.arg(1)?;
        let (dx, dy) = if rect == 0 {
            (0, 0)
        } else {
            (
                call.memory.read_u32(rect)? as i32,
                call.memory.read_u32(rect + 4)? as i32,
            )
        };
        (rect, call.arg(2)?, call.arg(3)?, dx, dy)
    };

    // The guest clears the screen with `Blt(NULL, NULL, DDBLT_COLORFILL | ...)`,
    // the only colour fill in the image. The fill colour sits at 0x50 in its
    // `DDBLTFX`: the routine at 0x00433198 builds the structure at `[ebp-0x68]`
    // and stores its colour argument at `[ebp-0x18]`, which is that offset.
    if method == "Blt" && call.arg(4)? & DDBLT_COLORFILL != 0 {
        let effects = call.arg(5)?;
        let colour = if effects == 0 {
            0
        } else {
            call.memory.read_u32(effects + FILL_COLOR)?
        };
        let shape = host
            .ddraw
            .surfaces
            .get(&this)
            .map_or_else(|| "?".to_string(), |surface| {
                format!("{}x{}x{}", surface.width, surface.height, surface.bits)
            });
        // `SHIMKIT_LOG_DRAW=fill` also wants the structure itself when the
        // colour read from it is not the one that reaches the screen: a field at
        // the wrong offset shows up as a grey fill of zero.
        let mut words = String::new();
        if effects != 0 {
            for offset in (0..0x60).step_by(4) {
                let value = call.memory.read_u32(effects + offset).unwrap_or(0);
                words.push_str(&format!(" {offset:02x}:{value:08x}"));
            }
        }
        trace(
            host,
            format!("fill {colour:#010x} rect {rect:#010x} on {this:#010x} ({shape}){words}"),
        );
        return fill(host, call, this, rect, colour);
    }
    {
        // A rectangle that is copied is only half the story: which surface it
        // came from and how big that surface is says whether the window is the
        // tile the guest meant or an empty corner of a bitmap that never loaded.
        let shape = |id: u32| {
            host.ddraw
                .surfaces
                .get(&id)
                .map_or_else(|| "?".to_string(), |surface| {
                    format!("{}x{}x{}", surface.width, surface.height, surface.bits)
                })
        };
        let window = if source_rect == 0 {
            "whole".to_string()
        } else {
            format!(
                "{},{},{},{}",
                call.memory.read_u32(source_rect).unwrap_or(0),
                call.memory.read_u32(source_rect + 4).unwrap_or(0),
                call.memory.read_u32(source_rect + 8).unwrap_or(0),
                call.memory.read_u32(source_rect + 12).unwrap_or(0)
            )
        };
        let (from, onto) = (shape(source), shape(this));
        // The pixel at the window's corner, so a rectangle that comes out black
        // says whether the source surface holds black or the copy dropped it.
        let sample = host.ddraw.surfaces.get(&source).map_or_else(
            || " source ?".to_string(),
            |surface| {
                let bytes = u32::from(surface.bits / 8).max(1);
                let (left, top) = if source_rect == 0 {
                    (0, 0)
                } else {
                    (
                        call.memory.read_u32(source_rect).unwrap_or(0),
                        call.memory.read_u32(source_rect + 4).unwrap_or(0),
                    )
                };
                let at = surface
                    .buffer
                    .wrapping_add(top.saturating_mul(surface.pitch))
                    .wrapping_add(left.saturating_mul(bytes));
                let mut value = 0u32;
                for index in 0..bytes.min(4) {
                    if let Ok(byte) = call.memory.read_bytes(at + index, 1) {
                        value |= u32::from(byte[0]) << (8 * index);
                    }
                }
                format!(" source pixel {value:#06x}")
            },
        );
        trace(
            host,
            format!(
                "blt {method} src={source:#010x} ({from}) dst={this:#010x} ({onto}) at {dx},{dy} window {window} transfer {:#x}{sample}",
                if method == "BltFast" { call.arg(5)? } else { call.arg(4)? }
            ),
        );
    }
    if source == 0 {
        // Without a fill flag a null source copies the destination onto itself,
        // which changes nothing.
        return Ok(Return(DD_OK));
    }

    let transfer = if method == "BltFast" {
        call.arg(5)?
    } else {
        call.arg(4)?
    };
    let (key_source, key_destination) = if method == "BltFast" {
        (
            transfer & DDBLTFAST_SRCCOLORKEY != 0,
            transfer & DDBLTFAST_DESTCOLORKEY != 0,
        )
    } else {
        (
            transfer & DDBLT_KEYSRC != 0,
            transfer & DDBLT_KEYDEST != 0,
        )
    };

    static LOGGING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let log = crate::shim::switch(&LOGGING, "SHIMKIT_LOG_BLITS");
    if log {
        let shape = host.ddraw.surfaces.get(&source).map(|surface| {
            format!("{}x{}x{}", surface.width, surface.height, surface.bits)
        });
        let onto = host.ddraw.surfaces.get(&this).map(|surface| {
            format!("{}x{}x{}", surface.width, surface.height, surface.bits)
        });
        let key = host
            .ddraw
            .surfaces
            .get(&source)
            .and_then(|surface| surface.src_key)
            .map_or("none".to_string(), |(low, high)| {
                format!("{low:#06x}..={high:#06x}")
            });
        let rect = if source_rect == 0 {
            "whole".to_string()
        } else {
            format!(
                "{},{},{},{}",
                call.memory.read_u32(source_rect).unwrap_or(0),
                call.memory.read_u32(source_rect + 4).unwrap_or(0),
                call.memory.read_u32(source_rect + 8).unwrap_or(0),
                call.memory.read_u32(source_rect + 12).unwrap_or(0)
            )
        };
        host.notice(format!(
            "{method} {shape:?} -> {onto:?} transfer {transfer:#x} at {dx},{dy} rect {rect} key {key}"
        ));
    }
    let Some(from) = host.ddraw.surfaces.get(&source) else {
        return Err(format!(
            "DirectDraw::{method} from {source:#010x}, which is not a surface this host created"
        ));
    };
    let Some(to) = host.ddraw.surfaces.get(&this) else {
        return Err(format!(
            "DirectDraw::{method} onto {this:#010x}, which is not a surface this host created"
        ));
    };
    // The source rectangle selects a window of the source surface; a null one is
    // the whole surface, which is what a sprite blit with no clipping means.
    //
    // Windows clips that window to the surface and answers `DDERR_INVALIDRECT`
    // when nothing is left of it, and this host does the same rather than
    // refusing the call: a sprite whose clip rectangle has emptied asks for
    // `862,112..0,0` -- Cave Story does it, then carries on with the frame -- so
    // an empty window is an ordinary answer, not a stop, and a window hanging
    // off the edge copies the part that exists instead of moving the sprite.
    let (mut sx, mut sy, mut width, mut height, shift_x, shift_y) = if source_rect == 0 {
        (0, 0, from.width, from.height, 0, 0)
    } else {
        let left = call.memory.read_u32(source_rect)? as i32;
        let top = call.memory.read_u32(source_rect + 4)? as i32;
        let right = call.memory.read_u32(source_rect + 8)? as i32;
        let bottom = call.memory.read_u32(source_rect + 12)? as i32;
        let surface_width = from.width as i32;
        let surface_height = from.height as i32;
        let clip_left = left.max(0);
        let clip_top = top.max(0);
        let clip_right = right.min(surface_width);
        let clip_bottom = bottom.min(surface_height);
        if clip_right <= clip_left || clip_bottom <= clip_top {
            return Ok(Return(DDERR_INVALIDRECT));
        }
        (
            clip_left as u32,
            clip_top as u32,
            (clip_right - clip_left) as u32,
            (clip_bottom - clip_top) as u32,
            // Cutting the window's left or top edge moves the copy with it, so
            // the pixels that do land stay where the guest put them.
            clip_left - left,
            clip_top - top,
        )
    };
    let (dx, dy) = (dx + shift_x, dy + shift_y);

    // Both blits clip the destination; a copy that runs off the edge is
    // truncated rather than refused. The game relies on it: its board sprites
    // are placed at y=433 on a 480-row screen, so their last rows fall off the
    // bottom, and Windows draws the part that fits.
    let mut left = dx;
    let mut top = dy;
    if left < 0 {
        let cut = left.unsigned_abs();
        if cut >= width {
            return Ok(Return(DD_OK));
        }
        sx += cut;
        width -= cut;
        left = 0;
    }
    if top < 0 {
        let cut = top.unsigned_abs();
        if cut >= height {
            return Ok(Return(DD_OK));
        }
        sy += cut;
        height -= cut;
        top = 0;
    }
    if left as u32 >= to.width || top as u32 >= to.height {
        // Entirely past the surface: nothing lands, which is not an error.
        return Ok(Return(DD_OK));
    }
    width = width.min(to.width - left as u32);
    height = height.min(to.height - top as u32);

    let bytes = usize::from(from.bits / 8).max(1);
    let row_bytes = width as usize * bytes;
    // A colour key is one pixel wide, so a keyed blit compares whole pixels.
    let pixel = |row: &[u8], column: usize| -> u32 {
        let at = column * bytes;
        let mut value = 0_u32;
        for index in (0..bytes).rev() {
            value = (value << 8) | u32::from(row[at + index]);
        }
        value
    };
    let from_base = from.buffer;
    let from_pitch = from.pitch;
    let to_base = to.buffer;
    let to_pitch = to.pitch;
    let src_key = if key_source { from.src_key } else { None };
    let dest_key = if key_destination { to.dest_key } else { None };
    // The two row buffers are reused across rows and calls: a moving frame is
    // hundreds of one-row sprite blits, and allocating two buffers for each was
    // work per row for no reason.
    let mut row_buffer = std::mem::take(&mut host.ddraw.row);
    let mut composed = std::mem::take(&mut host.ddraw.composed);
    // `sx` and `left` are pixel columns, and a truecolour surface holds more
    // than one byte to a pixel: an address is the column times the pixel size.
    // Cave Story draws at 640x480x16, so a column added raw lands half a sprite
    // away, which reads as a scrambled screen rather than a misplaced one.
    let pixel_bytes = bytes as u32;
    let mut copied = 0_usize;
    for row in 0..height {
        let source_at = from_base + (sy + row) * from_pitch + sx * pixel_bytes;
        let destination = to_base + (top as u32 + row) * to_pitch + left as u32 * pixel_bytes;
        if src_key.is_none() && dest_key.is_none() {
            call.memory.copy(source_at, destination, row_bytes)?;
            copied += width as usize;
            continue;
        }
        row_buffer.clear();
        row_buffer.extend_from_slice(call.memory.read_bytes(source_at, row_bytes)?);
        // A keyed blit leaves the pixels under the key alone, so the destination
        // row has to be read back and only the other pixels replaced.
        composed.clear();
        composed.extend_from_slice(call.memory.read_bytes(destination, row_bytes)?);
        for column in 0..width as usize {
            let source_pixel = pixel(&row_buffer, column);
            if let Some((low, high)) = src_key {
                if (low..=high).contains(&source_pixel) {
                    continue;
                }
            }
            if let Some((low, high)) = dest_key {
                if (low..=high).contains(&pixel(&composed, column)) {
                    continue;
                }
            }
            let at = column * bytes;
            composed[at..at + bytes].copy_from_slice(&row_buffer[at..at + bytes]);
            copied += 1;
        }
        call.memory.write_bytes(destination, &composed)?;
    }
    if log {
        // The first bytes of the source row, so "the copy found nothing" can be
        // told from "the copy read the wrong memory".
        let head = call
            .memory
            .read_bytes(from_base + sy * from_pitch + sx * pixel_bytes, (row_bytes).min(8))
            .map(|bytes| bytes.iter().map(|byte| format!("{byte:02x}")).collect::<String>())
            .unwrap_or_default();
        host.notice(format!(
            "blit copied {copied} of {} pixels ({method} onto {this:#010x}) from {from_base:#010x} at {sx},{sy} row {head} to {to_base:#010x} at {left},{top}",
            width as usize * height as usize
        ));
    }
    host.ddraw.row = row_buffer;
    host.ddraw.composed = composed;
    Ok(Return(DD_OK))
}

/// `SHIMKIT_LOG_DRAW` traces what the guest draws, which is how a region that
/// comes out the wrong colour is traced to the call that painted it: a rectangle
/// that is never filled and one filled with 0 look the same on screen.
fn trace(host: &mut Host, line: String) {
    let Ok(wanted) = std::env::var("SHIMKIT_LOG_DRAW") else {
        return;
    };
    // A screen that tiles its backdrop makes hundreds of blits a frame and a
    // handful of fills, so an unfiltered trace fills the notice list before the
    // frame that matters. A plain `1` means every call; anything else names a
    // prefix -- `fill`, `blt` -- so the list is spent on one kind of call
    // instead of the loudest one.
    let everything = wanted.is_empty()
        || wanted == "1"
        || wanted.eq_ignore_ascii_case("true")
        || wanted.eq_ignore_ascii_case("yes");
    if !everything && !line.starts_with(&wanted) {
        return;
    }
    // `SHIMKIT_LOG_DRAW_LIMIT` raises the cap for a run that has to see every
    // call of one frame rather than the first two frames of a scene.
    let limit = std::env::var("SHIMKIT_LOG_DRAW_LIMIT")
        .ok()
        .and_then(|text| text.trim().parse::<usize>().ok())
        .unwrap_or(600);
    if host.notices.len() < limit {
        host.notice(format!("draw {line}"));
    }
}

/// Paints one colour across a rectangle, which is `DDBLT_COLORFILL`.
///
/// The fill colour is a dword in `DDBLTFX`; on an 8-bit surface only its low
/// byte is the palette index, and on a truecolour one the pixel is as many bytes
/// as the surface holds.
fn fill(
    host: &mut Host,
    call: &mut Call<'_>,
    this: u32,
    rect: u32,
    colour: u32,
) -> Result<Action> {
    let Some(surface) = host.ddraw.surfaces.get(&this) else {
        return Err(format!("DirectDraw: fill of {this:#010x}, which is not a surface"));
    };
    let (left, top, right, bottom) = bounds(call, rect, surface)?;            let bytes = usize::from(surface.bits / 8).max(1);
            let mut pixel = vec![0_u8; bytes];
    for (index, byte) in colour.to_le_bytes().iter().take(bytes).enumerate() {
        pixel[index] = *byte;
    }
    let row: Vec<u8> = pixel
        .iter()
        .copied()
        .cycle()
        .take((right - left) as usize * bytes)
        .collect();
    for y in top as u32..bottom as u32 {
        let at = surface.buffer + y * surface.pitch + left as u32 * bytes as u32;
        call.memory.write_bytes(at, &row)?;
    }
    Ok(Return(DD_OK))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};

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
        let args = STACK_BASE + 0xc000;
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

    /// Creates the primary surface the way the game does, through the shim.
    fn direct_draw(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        let destination = STACK_BASE + 0xc100;
        assert_eq!(
            call_shim(host, memory, "DirectDrawCreate", &[0, destination, 0]).unwrap(),
            Return(DD_OK)
        );
        memory.read_u32(destination).unwrap()
    }

    fn surface_args(desc: u32, width: u32, height: u32, caps: u32, memory: &mut GuestMemory) {
        memory.write_u32(desc, DESC_SIZE).unwrap();
        memory.write_u32(desc + DESC_FLAGS, DDSD_HEIGHT | DDSD_WIDTH | DDSD_CAPS).unwrap();
        memory.write_u32(desc + DESC_HEIGHT, height).unwrap();
        memory.write_u32(desc + DESC_WIDTH, width).unwrap();
        memory.write_u32(desc + DESC_PITCH, 0).unwrap();
        memory.write_u32(desc + DESC_CAPS, caps).unwrap();
    }

    #[test]
    fn the_object_set_a_mode_and_created_surfaces() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let draw = direct_draw(&mut host, &mut memory);
        assert_eq!(thunks::at(memory.read_u32(memory.read_u32(draw).unwrap() + 24).unwrap()),
            Some((thunks::DDRAW, 6)));

        let args = STACK_BASE + 0xc200;
        let desc = STACK_BASE + 0xc300;
        let destination = STACK_BASE + 0xc400;

        // Primary, 640x480x8.
        surface_args(desc, 640, 480, DDSCAPS_PRIMARYSURFACE, &mut memory);
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        memory.write_u32(args + 12, 0).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        assert_eq!(serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap(), Return(DD_OK));
        let primary = call.memory.read_u32(destination).unwrap();
        assert!(primary > 0);

        // Back buffer.
        surface_args(desc, 640, 480, DDSCAPS_BACKBUFFER, &mut memory);
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        assert_eq!(serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap(), Return(DD_OK));

        assert_eq!(host.ddraw.surface_count(), 2);
        let (buffer, width, height) = host.ddraw.framebuffer().expect("a primary surface");
        assert_eq!((width, height), (640, 480));
        assert!(buffer > 0);
    }

    #[test]
    fn a_complex_primary_owns_the_back_buffer_the_guest_asks_for() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let draw = direct_draw(&mut host, &mut memory);
        let args = STACK_BASE + 0xd200;
        let desc = STACK_BASE + 0xd300;
        let destination = STACK_BASE + 0xd400;

        // What the image asks for: 640x480, primary, complex and flipping, so
        // DirectDraw creates the back buffer with it.
        surface_args(
            desc,
            640,
            480,
            DDSCAPS_PRIMARYSURFACE | DDSCAPS_COMPLEX | DDSCAPS_FLIP,
            &mut memory,
        );
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        memory.write_u32(args + 12, 0).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        let primary = call.memory.read_u32(destination).unwrap();

        // `GetAttachedSurface` takes a pointer to the DDSCAPS naming the buffer.
        let caps = STACK_BASE + 0xd500;
        let back = STACK_BASE + 0xd600;
        call.memory.write_u32(caps, DDSCAPS_BACKBUFFER).unwrap();
        memory.write_u32(args, primary).unwrap();
        memory.write_u32(args + 4, caps).unwrap();
        memory.write_u32(args + 8, back).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 12 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "GetAttachedSurface").unwrap(),
            Return(DD_OK)
        );
        let attached = call.memory.read_u32(back).unwrap();
        assert!(attached > 0, "a flipping primary has a back buffer");
        assert_ne!(attached, primary);
        assert_eq!(host.ddraw.surface_count(), 2);
        // Whichever surface is asked for, the attachment is the same buffer.
        assert_eq!(host.ddraw.backbuffer, Some(attached));
    }

    #[test]
    fn a_colour_fill_paints_the_whole_surface() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let draw = direct_draw(&mut host, &mut memory);
        let args = STACK_BASE + 0xd700;
        let desc = STACK_BASE + 0xd800;
        let destination = STACK_BASE + 0xd900;
        surface_args(desc, 8, 4, DDSCAPS_PRIMARYSURFACE, &mut memory);
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        let primary = call.memory.read_u32(destination).unwrap();

        // `Blt(NULL, NULL, DDBLT_COLORFILL, &fx)` with 7 in the fill colour.
        let effects = STACK_BASE + 0xda00;
        memory.write_u32(effects, 0x64).unwrap();
        memory.write_u32(effects + FILL_COLOR, 7).unwrap();
        memory.write_u32(args, primary).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, 0).unwrap();
        memory.write_u32(args + 12, 0).unwrap();
        memory.write_u32(args + 16, DDBLT_COLORFILL | 0x0100_0000).unwrap();
        memory.write_u32(args + 20, effects).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 24 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap(),
            Return(DD_OK)
        );

        let (buffer, width, height) = host.ddraw.framebuffer().unwrap();
        let pitch = host.ddraw.framebuffer_pitch();
        for row in 0..height {
            let bytes = call
                .memory
                .read_bytes(buffer + row * pitch, width as usize)
                .unwrap();
            assert!(bytes.iter().all(|&index| index == 7), "row {row}");
        }
    }

    #[test]
    fn a_lock_hands_out_the_surfaces_own_pitch_and_a_stable_pointer() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let draw = direct_draw(&mut host, &mut memory);
        let args = STACK_BASE + 0xc500;
        let desc = STACK_BASE + 0xc600;
        let destination = STACK_BASE + 0xc700;
        surface_args(desc, 8, 4, DDSCAPS_OFFSCREENPLAIN, &mut memory);
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        let surface = call.memory.read_u32(destination).unwrap();

        let lock_desc = STACK_BASE + 0xc800;
        let rect = STACK_BASE + 0xc900;
        memory.write_u32(rect, 2).unwrap();
        memory.write_u32(rect + 4, 1).unwrap();
        memory.write_u32(rect + 8, 6).unwrap();
        memory.write_u32(rect + 12, 3).unwrap();
        memory.write_u32(args, surface).unwrap();
        memory.write_u32(args + 4, rect).unwrap();
        memory.write_u32(args + 8, lock_desc).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        assert_eq!(serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Lock").unwrap(), Return(DD_OK));
        let pitch = call.memory.read_u32(lock_desc + DESC_PITCH).unwrap();
        let pointer = call.memory.read_u32(lock_desc + DESC_SURFACE).unwrap();
        assert_eq!(pitch, 8);
        assert_eq!(call.memory.read_u32(lock_desc + DESC_WIDTH).unwrap(), 4);
        assert_eq!(call.memory.read_u32(lock_desc + DESC_HEIGHT).unwrap(), 2);
        // The rectangle starts one row down and two bytes in.
        let base = host.ddraw.surfaces.get(&surface).unwrap().buffer;
        assert_eq!(pointer, base + 8 + 2);
    }

    #[test]
    fn a_lock_describes_the_pixels_it_hands_out() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let draw = direct_draw(&mut host, &mut memory);
        // A 16-bit display, so a surface the guest creates afterwards is one
        // whose pixels a conversion masks with the bit count it reads back.
        let args = STACK_BASE + 0xd000;
        let desc = STACK_BASE + 0xd100;
        let destination = STACK_BASE + 0xd200;
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, 640).unwrap();
        memory.write_u32(args + 8, 480).unwrap();
        memory.write_u32(args + 12, 16).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 20 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW, "SetDisplayMode").unwrap(),
            Return(DD_OK)
        );

        surface_args(desc, 8, 4, DDSCAPS_OFFSCREENPLAIN, &mut memory);
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        let surface = call.memory.read_u32(destination).unwrap();

        let lock_desc = STACK_BASE + 0xd300;
        memory.write_u32(args, surface).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, lock_desc).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Lock").unwrap(),
            Return(DD_OK)
        );
        // Cave Story masks the pixel it reads through a lock with this count to
        // find the colour it wrote, so a zero one turns every fade black.
        assert_eq!(call.memory.read_u32(lock_desc + DESC_BITS).unwrap(), 16);
        assert_eq!(call.memory.read_u32(lock_desc + DESC_RED_MASK).unwrap(), 0xf800);
        assert_eq!(
            call.memory.read_u32(lock_desc + DESC_FLAGS).unwrap() & DDSD_PIXELFORMAT,
            DDSD_PIXELFORMAT,
            "the format in the descriptor is one the guest is told to read"
        );
    }

    #[test]
    fn a_flip_copies_the_back_buffer_onto_the_primary() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let draw = direct_draw(&mut host, &mut memory);
        let args = STACK_BASE + 0xca00;
        let desc = STACK_BASE + 0xcb00;
        let destination = STACK_BASE + 0xcc00;

        surface_args(desc, 4, 2, DDSCAPS_PRIMARYSURFACE, &mut memory);
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();

        surface_args(desc, 4, 2, DDSCAPS_BACKBUFFER, &mut memory);
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        let back = call.memory.read_u32(destination).unwrap();

        let back_buffer = host.ddraw.surfaces.get(&back).unwrap().buffer;
        call.memory
            .write_bytes(back_buffer, &[1, 2, 3, 4, 5, 6, 7, 8])
            .unwrap();

        memory.write_u32(args, 0).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, 0).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 12 };
        assert_eq!(serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Flip").unwrap(), Return(DD_OK));
        assert_eq!(host.ddraw.flips(), 1);
        let (buffer, _, _) = host.ddraw.framebuffer().unwrap();
        assert_eq!(
            call.memory.read_bytes(buffer, 8).unwrap(),
            &[1, 2, 3, 4, 5, 6, 7, 8]
        );
    }

    /// Creates one 8x4 surface through `CreateSurface`, the way the game does.
    fn eight_by_four(host: &mut Host, memory: &mut GuestMemory, caps: u32) -> u32 {
        let draw = direct_draw(host, memory);
        let args = STACK_BASE + 0xe000;
        let desc = STACK_BASE + 0xe100;
        let destination = STACK_BASE + 0xe200;
        surface_args(desc, 8, 4, caps, memory);
        memory.write_u32(args, draw).unwrap();
        memory.write_u32(args + 4, desc).unwrap();
        memory.write_u32(args + 8, destination).unwrap();
        let mut call = Call { memory, args, arg_bytes: 16 };
        serve(host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        call.memory.read_u32(destination).unwrap()
    }

    #[test]
    fn releasing_a_surface_hands_its_pixels_back_to_the_allocator() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let surface = eight_by_four(&mut host, &mut memory, DDSCAPS_OFFSCREENPLAIN);
        let (buffer, size) = {
            let surface = host.ddraw.surfaces.get(&surface).unwrap();
            (surface.buffer, surface.size)
        };

        let args = STACK_BASE + 0xe600;
        memory.write_u32(args, surface).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 4 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Release").unwrap(),
            Return(0),
            "the last reference releases the surface"
        );
        assert!(!host.ddraw.surfaces.contains_key(&surface), "the surface is gone");

        // Its pixels are available again: the next allocation of that size lands
        // on the same block instead of advancing the arena, which is what makes a
        // session that creates a surface per tile fit in the heap at all.
        assert_eq!(host.allocate(size, 4).unwrap(), buffer);
    }

    #[test]
    fn a_keyed_blit_leaves_the_pixels_under_the_key_alone() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let sprite = eight_by_four(&mut host, &mut memory, DDSCAPS_OFFSCREENPLAIN);
        let page = eight_by_four(&mut host, &mut memory, DDSCAPS_PRIMARYSURFACE);

        // The sprite is colour 5 with a 3x2 hole of colour 0, the key.
        let (sprite_buffer, sprite_pitch) = {
            let surface = host.ddraw.surfaces.get(&sprite).unwrap();
            (surface.buffer, surface.pitch)
        };
        let mut pattern = vec![5_u8; (sprite_pitch * 4) as usize];
        for row in 1..3_u32 {
            for column in 2..5_u32 {
                pattern[(row * sprite_pitch + column) as usize] = 0;
            }
        }
        memory.write_bytes(sprite_buffer, &pattern).unwrap();

        // Give the sprite that key, then paint the page and blit it on top.
        let key = STACK_BASE + 0xe300;
        let args = STACK_BASE + 0xe400;
        memory.write_u32(key, 0).unwrap();
        memory.write_u32(key + 4, 0).unwrap();
        memory.write_u32(args, sprite).unwrap();
        memory.write_u32(args + 4, DDCKEY_SRCBLT).unwrap();
        memory.write_u32(args + 8, key).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 12 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "SetColorKey").unwrap(),
            Return(DD_OK)
        );

        let effects = STACK_BASE + 0xe500;
        memory.write_u32(effects, 0x64).unwrap();
        memory.write_u32(effects + FILL_COLOR, 7).unwrap();
        memory.write_u32(args, page).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, 0).unwrap();
        memory.write_u32(args + 12, 0).unwrap();
        memory.write_u32(args + 16, DDBLT_COLORFILL).unwrap();
        memory.write_u32(args + 20, effects).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 24 };
        serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap();

        memory.write_u32(args, page).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, sprite).unwrap();
        memory.write_u32(args + 12, 0).unwrap();
        memory.write_u32(args + 16, DDBLT_KEYSRC).unwrap();
        memory.write_u32(args + 20, 0).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 24 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap(),
            Return(DD_OK)
        );

        let (buffer, width, height) = host.ddraw.framebuffer().unwrap();
        let pitch = host.ddraw.framebuffer_pitch();
        for row in 0..height {
            let pixels = call
                .memory
                .read_bytes(buffer + row * pitch, width as usize)
                .unwrap();
            for (column, pixel) in pixels.iter().enumerate() {
                let keyed = (1..3).contains(&row) && (2..5).contains(&(column as u32));
                let expected = if keyed { 7 } else { 5 };
                assert_eq!(*pixel, expected, "row {row} column {column}");
            }
        }

        // And the guest can read the key back.
        memory.write_u32(args, sprite).unwrap();
        memory.write_u32(args + 4, DDCKEY_SRCBLT).unwrap();
        memory.write_u32(args + 8, key).unwrap();
        memory.write_u32(key, 9).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 12 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "GetColorKey").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(call.memory.read_u32(key).unwrap(), 0);
        assert_eq!(call.memory.read_u32(key + 4).unwrap(), 0);
    }

    /// Writes the four fields of an `RECT` for a blit to name.
    fn rectangle(at: u32, edges: (i32, i32, i32, i32), memory: &mut GuestMemory) {
        memory.write_u32(at, edges.0 as u32).unwrap();
        memory.write_u32(at + 4, edges.1 as u32).unwrap();
        memory.write_u32(at + 8, edges.2 as u32).unwrap();
        memory.write_u32(at + 12, edges.3 as u32).unwrap();
    }

    /// A sprite whose byte at row r, column c is `r * 8 + c`, so a copy that
    /// lands in the wrong place is visible rather than merely wrong.
    fn numbered_sprite(host: &mut Host, memory: &mut GuestMemory, surface: u32) {
        let (buffer, pitch) = {
            let surface = host.ddraw.surfaces.get(&surface).unwrap();
            (surface.buffer, surface.pitch)
        };
        let mut pattern = vec![0_u8; (pitch * 4) as usize];
        for row in 0..4_u32 {
            for column in 0..8_u32 {
                pattern[(row * pitch + column) as usize] = (row * 8 + column) as u8;
            }
        }
        memory.write_bytes(buffer, &pattern).unwrap();
    }

    /// Paints the page colour 7, so what a blit left alone and what it drew are
    /// both readable.
    fn paint(host: &mut Host, memory: &mut GuestMemory, page: u32) {
        let effects = STACK_BASE + 0xe700;
        let args = STACK_BASE + 0xe400;
        memory.write_u32(effects, 0x64).unwrap();
        memory.write_u32(effects + FILL_COLOR, 7).unwrap();
        memory.write_u32(args, page).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, 0).unwrap();
        memory.write_u32(args + 12, 0).unwrap();
        memory.write_u32(args + 16, DDBLT_COLORFILL).unwrap();
        memory.write_u32(args + 20, effects).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: 24,
        };
        serve(host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap();
    }

    /// `Blt(&dest, sprite, &source, 0, 0)` and the action it answered with.
    fn blit(
        host: &mut Host,
        memory: &mut GuestMemory,
        page: u32,
        sprite: u32,
        source_rect: u32,
        at: (i32, i32),
    ) -> Action {
        let dest = STACK_BASE + 0xe800;
        let args = STACK_BASE + 0xe400;
        rectangle(dest, (at.0, at.1, 0, 0), memory);
        memory.write_u32(args, page).unwrap();
        memory.write_u32(args + 4, dest).unwrap();
        memory.write_u32(args + 8, sprite).unwrap();
        memory.write_u32(args + 12, source_rect).unwrap();
        memory.write_u32(args + 16, 0).unwrap();
        memory.write_u32(args + 20, 0).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: 24,
        };
        serve(host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap()
    }

    /// One row of the presented frame.
    fn page_row(host: &Host, memory: &mut GuestMemory, row: u32) -> Vec<u8> {
        let (buffer, width, _) = host.ddraw.framebuffer().unwrap();
        let pitch = host.ddraw.framebuffer_pitch();
        memory
            .read_bytes(buffer + row * pitch, width as usize)
            .unwrap()
            .to_vec()
    }

    #[test]
    fn a_blit_whose_source_rectangle_is_empty_draws_nothing() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let sprite = eight_by_four(&mut host, &mut memory, DDSCAPS_OFFSCREENPLAIN);
        let page = eight_by_four(&mut host, &mut memory, DDSCAPS_PRIMARYSURFACE);
        numbered_sprite(&mut host, &mut memory, sprite);
        paint(&mut host, &mut memory, page);

        // What a sprite entirely outside its clip window asks for: the right and
        // bottom edges before the origin. Windows answers `DDERR_INVALIDRECT` and
        // copies nothing; refusing the call would stop a game mid-frame.
        let source = STACK_BASE + 0xe900;
        rectangle(source, (5, 2, 0, 0), &mut memory);
        assert_eq!(
            blit(&mut host, &mut memory, page, sprite, source, (0, 0)),
            Return(DDERR_INVALIDRECT)
        );
        for row in 0..4 {
            assert!(
                page_row(&host, &mut memory, row).iter().all(|&index| index == 7),
                "row {row} is untouched"
            );
        }
    }

    #[test]
    fn a_source_rectangle_past_the_edge_copies_what_exists_at_the_right_place() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let sprite = eight_by_four(&mut host, &mut memory, DDSCAPS_OFFSCREENPLAIN);
        let page = eight_by_four(&mut host, &mut memory, DDSCAPS_PRIMARYSURFACE);
        numbered_sprite(&mut host, &mut memory, sprite);
        let source = STACK_BASE + 0xe900;

        // Hanging off the right and bottom: four columns by two rows exist.
        paint(&mut host, &mut memory, page);
        rectangle(source, (4, 2, 99, 99), &mut memory);
        assert_eq!(
            blit(&mut host, &mut memory, page, sprite, source, (0, 0)),
            Return(DD_OK)
        );
        assert_eq!(page_row(&host, &mut memory, 0), &[4, 5, 6, 7, 7, 7, 7, 7]);
        assert_eq!(page_row(&host, &mut memory, 1), &[12, 13, 14, 15, 7, 7, 7, 7]);
        assert_eq!(page_row(&host, &mut memory, 2), &[7; 8]);

        // Hanging off the left: the two columns that do exist land two pixels to
        // the right of where the copy would have started, not at its start.
        paint(&mut host, &mut memory, page);
        rectangle(source, (-2, 0, 3, 2), &mut memory);
        assert_eq!(
            blit(&mut host, &mut memory, page, sprite, source, (0, 0)),
            Return(DD_OK)
        );
        assert_eq!(page_row(&host, &mut memory, 0), &[7, 7, 0, 1, 2, 7, 7, 7]);
        assert_eq!(page_row(&host, &mut memory, 1), &[7, 7, 8, 9, 10, 7, 7, 7]);
        assert_eq!(page_row(&host, &mut memory, 2), &[7; 8]);
    }

    #[test]
    fn set_entries_colours_the_palette() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let entries = STACK_BASE + 0xcd00;
        // `SetEntries` is given a pointer to the first entry it sets, so the
        // colours start at the pointer, not at the palette's base.
        memory.write_bytes(entries, &[255, 0, 0, 0, 0, 255, 0, 0]).unwrap();
        let args = STACK_BASE + 0xce00;
        memory.write_u32(args, 0x1234).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, 3).unwrap();
        memory.write_u32(args + 12, 2).unwrap();
        memory.write_u32(args + 16, entries).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 20 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_PALETTE, "SetEntries").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(host.ddraw.palette()[3], [255, 0, 0]);
        assert_eq!(host.ddraw.palette()[4], [0, 255, 0]);
        assert_eq!(host.ddraw.palette()[5], [0, 0, 0]);
    }

    #[test]
    fn a_fade_bumps_the_palette_revision_without_a_flip() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The logo and every screen change fade by rewriting entries on the same
        // presented frame, so the page has to be told the palette moved even
        // though the flip count stands still.
        let entries = STACK_BASE + 0xcd00;
        let args = STACK_BASE + 0xce00;
        let before = host.ddraw.palette_revision();
        let flips = host.ddraw.flips();

        for step in 1..=4_u8 {
            memory
                .write_bytes(entries, &[step * 8, step * 8, step * 8, 0])
                .unwrap();
            memory.write_u32(args, 0x1234).unwrap();
            memory.write_u32(args + 4, 0).unwrap();
            memory.write_u32(args + 8, 7).unwrap();
            memory.write_u32(args + 12, 1).unwrap();
            memory.write_u32(args + 16, entries).unwrap();
            let mut call = Call {
                memory: &mut memory,
                args,
                arg_bytes: 20,
            };
            assert_eq!(
                serve(&mut host, &mut call, thunks::DDRAW_PALETTE, "SetEntries").unwrap(),
                Return(DD_OK)
            );
        }

        assert_eq!(host.ddraw.palette()[7], [32, 32, 32]);
        assert_eq!(host.ddraw.palette_revision(), before + 4);
        assert_eq!(host.ddraw.flips(), flips);

        // Rewriting the same colour is not a change, so an idle fade does not
        // keep the page repainting the frame it already shows.
        memory.write_u32(args + 12, 1).unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 20,
        };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_PALETTE, "SetEntries").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(host.ddraw.palette_revision(), before + 4);
    }

    #[test]
    fn a_palette_without_allow256_keeps_its_reserved_ends() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // The guest asks for `DDPCAPS_8BIT` alone, then writes all 256 entries
        // at once -- the title palette's entry 0 is the artwork's pink colour
        // key, which must not reach the screen.
        let colours = STACK_BASE + 0xcf00;
        let mut bytes = vec![200_u8, 100, 100, 0];
        bytes.resize(256 * 4, 9);
        memory.write_bytes(colours, &bytes).unwrap();

        let args = STACK_BASE + 0xd000;
        memory.write_u32(args, 0x1234).unwrap();
        memory.write_u32(args + 4, 0x4).unwrap();
        memory.write_u32(args + 8, colours).unwrap();
        memory.write_u32(args + 12, STACK_BASE + 0xd100).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW, "CreatePalette").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(host.ddraw.palette()[0], [0, 0, 0]);
        assert_eq!(host.ddraw.palette()[255], [255, 255, 255]);
        assert_eq!(host.ddraw.palette()[1], [9, 9, 9]);

        let palette = call.memory.read_u32(STACK_BASE + 0xd100).unwrap();
        memory.write_u32(args, palette).unwrap();
        memory.write_u32(args + 4, 0).unwrap();
        memory.write_u32(args + 8, 0).unwrap();
        memory.write_u32(args + 12, 256).unwrap();
        memory.write_u32(args + 16, colours).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 20 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_PALETTE, "SetEntries").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(host.ddraw.palette()[0], [0, 0, 0]);
        assert_eq!(host.ddraw.palette()[254], [9, 9, 9]);
        assert_eq!(host.ddraw.palette()[255], [255, 255, 255]);
    }

    #[test]
    fn allow256_lets_the_guest_set_every_entry() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let colours = STACK_BASE + 0xd200;
        let mut bytes = vec![200_u8, 100, 100, 0];
        bytes.resize(256 * 4, 9);
        memory.write_bytes(colours, &bytes).unwrap();
        let args = STACK_BASE + 0xd300;
        memory.write_u32(args, 0x1234).unwrap();
        memory.write_u32(args + 4, 0x44).unwrap();
        memory.write_u32(args + 8, colours).unwrap();
        memory.write_u32(args + 12, STACK_BASE + 0xd400).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW, "CreatePalette").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(host.ddraw.palette()[0], [200, 100, 100]);
        assert_eq!(host.ddraw.palette()[255], [9, 9, 9]);
    }

    #[test]
    fn a_truecolour_mode_is_kept_and_a_mode_a_page_cannot_read_is_refused() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let args = STACK_BASE + 0xcf00;
        for depth in [16, 32] {
            memory.write_u32(args, 0x1234).unwrap();
            memory.write_u32(args + 4, 640).unwrap();
            memory.write_u32(args + 8, 480).unwrap();
            memory.write_u32(args + 12, depth).unwrap();
            let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
            assert_eq!(
                serve(&mut host, &mut call, thunks::DDRAW, "SetDisplayMode").unwrap(),
                Return(DD_OK),
                "{depth} bits per pixel"
            );
            assert_eq!(host.ddraw.depth, depth as u8);
        }
        // 15 and 24 put a pixel in bytes a page cannot read as a colour.
        for depth in [15, 24] {
            memory.write_u32(args + 12, depth).unwrap();
            let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
            let error = serve(&mut host, &mut call, thunks::DDRAW, "SetDisplayMode").unwrap_err();
            assert!(error.contains("bits per pixel"), "{error}");
        }
    }

    #[test]
    fn a_sixteen_bit_surface_takes_two_bytes_a_pixel() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let args = STACK_BASE + 0xdb00;
        memory.write_u32(args, 0x1234).unwrap();
        memory.write_u32(args + 4, 8).unwrap();
        memory.write_u32(args + 8, 4).unwrap();
        memory.write_u32(args + 12, 16).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "SetDisplayMode").unwrap();

        // A descriptor with no pitch of its own: DirectDraw computes it from the
        // width and the pixel depth, and so does this host.
        let draw = direct_draw(&mut host, &mut memory);
        let create = STACK_BASE + 0xdc00;
        let desc = STACK_BASE + 0xdd00;
        let destination = STACK_BASE + 0xde00;
        surface_args(desc, 8, 4, DDSCAPS_PRIMARYSURFACE, &mut memory);
        memory.write_u32(create, draw).unwrap();
        memory.write_u32(create + 4, desc).unwrap();
        memory.write_u32(create + 8, destination).unwrap();
        let mut call = Call { memory: &mut memory, args: create, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "CreateSurface").unwrap();
        assert_eq!(host.ddraw.framebuffer_pitch(), 16, "8 pixels of two bytes");
        assert_eq!(host.ddraw.bits(), 16);

        // A colour fill writes a two-byte pixel across the whole row.
        let effects = STACK_BASE + 0xdf00;
        let primary = call.memory.read_u32(destination).unwrap();
        memory.write_u32(effects, 0x64).unwrap();
        memory.write_u32(effects + FILL_COLOR, 0x1234).unwrap();
        memory.write_u32(create, primary).unwrap();
        memory.write_u32(create + 4, 0).unwrap();
        memory.write_u32(create + 8, 0).unwrap();
        memory.write_u32(create + 12, 0).unwrap();
        memory.write_u32(create + 16, DDBLT_COLORFILL | 0x0100_0000).unwrap();
        memory.write_u32(create + 20, effects).unwrap();
        let mut call = Call { memory: &mut memory, args: create, arg_bytes: 24 };
        serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap();
        let (buffer, _, _) = host.ddraw.framebuffer().unwrap();
        let pitch = host.ddraw.framebuffer_pitch();
        let row = call.memory.read_bytes(buffer, pitch as usize).unwrap();
        assert_eq!(&row[..4], &[0x34, 0x12, 0x34, 0x12], "the fill colour, twice");
    }

    #[test]
    fn a_sixteen_bit_blit_and_fill_count_columns_in_pixels() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // Cave Story runs at 640x480x16 and draws its artwork with `Blt`, so a
        // column of that surface is two bytes. Counting it as one put every
        // sprite half a picture away, which reads as a scrambled screen.
        let args = STACK_BASE + 0xf000;
        memory.write_u32(args, 0x1234).unwrap();
        memory.write_u32(args + 4, 8).unwrap();
        memory.write_u32(args + 8, 4).unwrap();
        memory.write_u32(args + 12, 16).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        serve(&mut host, &mut call, thunks::DDRAW, "SetDisplayMode").unwrap();

        let sprite = eight_by_four(&mut host, &mut memory, DDSCAPS_OFFSCREENPLAIN);
        let page = eight_by_four(&mut host, &mut memory, DDSCAPS_PRIMARYSURFACE);
        let (sprite_buffer, sprite_pitch) = {
            let surface = host.ddraw.surfaces.get(&sprite).unwrap();
            (surface.buffer, surface.pitch)
        };
        assert_eq!(sprite_pitch, 16, "8 pixels of two bytes");
        // Three pixels of colour on the sprite's second row, from column 1.
        for (column, value) in [(1_u32, 0x0102_u16), (2, 0x0304), (3, 0x0506)] {
            memory
                .write_bytes(sprite_buffer + sprite_pitch + column * 2, &value.to_le_bytes())
                .unwrap();
        }

        let blit = STACK_BASE + 0xf400;
        let dest = STACK_BASE + 0xf500;
        let source = STACK_BASE + 0xf600;
        for (at, values) in [(dest, [2_u32, 1, 5, 2]), (source, [1, 1, 4, 2])] {
            for (index, value) in values.iter().enumerate() {
                memory.write_u32(at + index as u32 * 4, *value).unwrap();
            }
        }
        memory.write_u32(blit, page).unwrap();
        memory.write_u32(blit + 4, dest).unwrap();
        memory.write_u32(blit + 8, sprite).unwrap();
        memory.write_u32(blit + 12, source).unwrap();
        memory.write_u32(blit + 16, 0).unwrap();
        memory.write_u32(blit + 20, 0).unwrap();
        let mut call = Call { memory: &mut memory, args: blit, arg_bytes: 24 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap(),
            Return(DD_OK)
        );
        let (page_buffer, page_pitch) = {
            let surface = host.ddraw.surfaces.get(&page).unwrap();
            (surface.buffer, surface.pitch)
        };
        assert_eq!(
            call.memory.read_bytes(page_buffer + page_pitch + 2 * 2, 2).unwrap(),
            &0x0102_u16.to_le_bytes(),
            "the sprite's column 1 landed on the page's column 2"
        );
        assert_eq!(
            call.memory.read_bytes(page_buffer + page_pitch + 4 * 2, 2).unwrap(),
            &0x0506_u16.to_le_bytes()
        );

        // A fill of a sub-rectangle starts at its own column too.
        let effects = STACK_BASE + 0xf700;
        let fill_rect = STACK_BASE + 0xf800;
        for (index, value) in [6_u32, 2, 8, 3].iter().enumerate() {
            memory.write_u32(fill_rect + index as u32 * 4, *value).unwrap();
        }
        memory.write_u32(effects, 0x64).unwrap();
        memory.write_u32(effects + FILL_COLOR, 0x0a0b).unwrap();
        memory.write_u32(blit, page).unwrap();
        memory.write_u32(blit + 4, fill_rect).unwrap();
        memory.write_u32(blit + 8, 0).unwrap();
        memory.write_u32(blit + 12, 0).unwrap();
        memory.write_u32(blit + 16, DDBLT_COLORFILL).unwrap();
        memory.write_u32(blit + 20, effects).unwrap();
        let mut call = Call { memory: &mut memory, args: blit, arg_bytes: 24 };
        assert_eq!(
            serve(&mut host, &mut call, thunks::DDRAW_SURFACE, "Blt").unwrap(),
            Return(DD_OK)
        );
        assert_eq!(
            call.memory.read_bytes(page_buffer + 2 * page_pitch + 6 * 2, 2).unwrap(),
            &0x0a0b_u16.to_le_bytes(),
            "column 6 of the filled row carries the fill colour"
        );
        assert_eq!(
            call.memory.read_bytes(page_buffer + 2 * page_pitch + 5 * 2, 2).unwrap(),
            &[0, 0],
            "the column before it is untouched"
        );
    }
}
