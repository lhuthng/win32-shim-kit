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
//! - `GetDC` has no GDI to hand out and faults; if the guest needs it, that is the
//!   M3 text work, and the stop says so.

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

// DDSURFACEDESC offsets (DX5, 108 bytes).
const DESC_SIZE: u32 = 108;
const DESC_FLAGS: u32 = 4;
const DESC_HEIGHT: u32 = 8;
const DESC_WIDTH: u32 = 12;
const DESC_PITCH: u32 = 16;
const DESC_SURFACE: u32 = 36;
const DESC_CAPS: u32 = 104;

// DDSD_* flags the guest sets.
const DDSD_CAPS: u32 = 0x1;
const DDSD_HEIGHT: u32 = 0x2;
const DDSD_WIDTH: u32 = 0x4;
const DDSD_PITCH: u32 = 0x8;
const DDSD_LPSURFACE: u32 = 0x100;

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
    /// Guest address of the pixels, `pitch * height` bytes the host allocated.
    buffer: u32,
    caps: u32,
    /// Locked rectangles: the guest may lock and unlock in a nested order, and the
    /// pointer only has to stay stable, which a bump-allocated buffer gives us.
    locks: u32,
    /// `DDCKEY_SRCBLT`: the palette indices, low to high, that are transparent
    /// when this surface is the source of a keyed blit.
    src_key: Option<(u8, u8)>,
    /// `DDCKEY_DESTBLT`: destination pixels inside this range are left alone.
    dest_key: Option<(u8, u8)>,
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
    /// Bumped every time an entry's colour actually changes. A fade is a palette
    /// written frame after frame *without* a flip, so a page that redraws only on
    /// `flips` would show the one frame it caught and nothing after it: the
    /// publisher logo flashing past black is this counter's absence.
    palette_revision: u32,
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
            palette_revision: 0,
            depth,
        }
    }
}

impl DirectDraw {
    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
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
    pub fn palette_revision(&self) -> u32 {
        self.palette_revision
    }

    /// The primary surface's pixels, which is what `Flip` presents.
    pub fn framebuffer(&self) -> Option<(u32, u32, u32)> {
        let primary = self.primary?;
        let surface = self.surfaces.get(&primary)?;
        Some((surface.buffer, surface.width, surface.height))
    }

    /// Bytes per row of the primary surface, which is its width for a
    /// linear 8-bit surface but is read from the surface rather than assumed.
    pub fn framebuffer_pitch(&self) -> u32 {
        self.primary
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
            .and_then(|handle| self.surfaces.get(&handle))
            .map(|surface| surface.buffer)
    }

    pub fn back_buffer(&self) -> Option<u32> {
        self.backbuffer
            .and_then(|handle| self.surfaces.get(&handle))
            .map(|surface| surface.buffer)
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
    caps: u32,
}

fn read_desc(call: &Call<'_>, address: u32, default: (u32, u32)) -> Result<SurfaceDesc> {
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
        pitch: if pitch == 0 { width } else { pitch },
        caps,
    })
}

fn write_desc(desc: &mut Call<'_>, address: u32, surface: &Surface) -> Result<()> {
    desc.zero(address, DESC_SIZE)?;
    desc.write_u32(address, DESC_SIZE)?;
    desc.write_u32(
        address + DESC_FLAGS,
        DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PITCH | DDSD_LPSURFACE,
    )?;
    desc.write_u32(address + DESC_HEIGHT, surface.height)?;
    desc.write_u32(address + DESC_WIDTH, surface.width)?;
    desc.write_u32(address + DESC_PITCH, surface.pitch)?;
    desc.write_u32(address + DESC_SURFACE, surface.buffer)?;
    desc.write_u32(address + DESC_CAPS, surface.caps)?;
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
            if depth != 8 {
                return Err(format!(
                    "DirectDraw asked for {depth} bits per pixel; this host presents 8"
                ));
            }
            host.ddraw.width = width;
            host.ddraw.height = height;
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
            let desc = read_desc(call, call.arg(1)?, (host.ddraw.width, host.ddraw.height))?;
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
                host.ddraw.primary = Some(object);
                if complex && host.ddraw.backbuffer.is_none() {
                    // A complex or flipping primary owns one back buffer, which the
                    // guest reaches with `GetAttachedSurface` rather than by
                    // creating it: DirectDraw allocated it with the primary.
                    let attached = Surface {
                        width: desc.width,
                        height: desc.height,
                        pitch: desc.pitch,
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
                    pitch: host.ddraw.width,
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
            if let Some(surface) = surface_of(host, call) {
                if surface.locks > 0 {
                    surface.locks -= 1;
                }
            }
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
        (thunks::DDRAW_SURFACE, "Blt") | (thunks::DDRAW_SURFACE, "BltFast") => blit(host, call, name),
        (thunks::DDRAW_SURFACE, "GetDC") | (thunks::DDRAW_SURFACE, "ReleaseDC") => {
            Err("IDirectDrawSurface::GetDC needs GDI, which is the M3 text work".to_string())
        }

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
    let offset = top as u32 * surface.pitch + left as u32;
    let pointer = surface.buffer + offset;
    let pitch = surface.pitch;
    if let Some(surface) = host.ddraw.surfaces.get_mut(&this) {
        surface.locks += 1;
    }
    call.zero(destination, DESC_SIZE)?;
    call.write_u32(destination, DESC_SIZE)?;
    call.write_u32(
        destination + DESC_FLAGS,
        DDSD_CAPS | DDSD_HEIGHT | DDSD_WIDTH | DDSD_PITCH | DDSD_LPSURFACE,
    )?;
    call.write_u32(destination + DESC_HEIGHT, height)?;
    call.write_u32(destination + DESC_WIDTH, width)?;
    call.write_u32(destination + DESC_PITCH, pitch)?;
    call.write_u32(destination + DESC_SURFACE, pointer)?;
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
    let row_bytes = source.width as usize;
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
        let low = call.memory.read_u32(key)? as u8;
        let high = call.memory.read_u32(key + 4)? as u8;
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
    call.write_u32(destination, u32::from(low))?;
    call.write_u32(destination + 4, u32::from(high))?;
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
            call.memory.read_u32(effects + FILL_COLOR)? as u8
        };
        return fill(host, call, this, rect, colour);
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
    let (mut sx, mut sy, mut width, mut height) = if source_rect == 0 {
        (0, 0, from.width, from.height)
    } else {
        let left = call.memory.read_u32(source_rect)? as i32;
        let top = call.memory.read_u32(source_rect + 4)? as i32;
        let right = call.memory.read_u32(source_rect + 8)? as i32;
        let bottom = call.memory.read_u32(source_rect + 12)? as i32;
        if left < 0
            || top < 0
            || right < left
            || bottom < top
            || right > from.width as i32
            || bottom > from.height as i32
        {
            return Err(format!(
                "DirectDraw::{method}: source rectangle {left},{top}..{right},{bottom} is \
                 outside a {}x{} surface",
                from.width, from.height
            ));
        }
        (
            left as u32,
            top as u32,
            (right - left) as u32,
            (bottom - top) as u32,
        )
    };

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

    let row_bytes = width as usize;
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
    for row in 0..height {
        let source_at = from_base + (sy + row) * from_pitch + sx;
        let destination = to_base + (top as u32 + row) * to_pitch + left as u32;
        if src_key.is_none() && dest_key.is_none() {
            call.memory.copy(source_at, destination, row_bytes)?;
            continue;
        }
        row_buffer.clear();
        row_buffer.extend_from_slice(call.memory.read_bytes(source_at, row_bytes)?);
        // A keyed blit leaves the pixels under the key alone, so the destination
        // row has to be read back and only the other pixels replaced.
        composed.clear();
        composed.extend_from_slice(call.memory.read_bytes(destination, row_bytes)?);
        for (column, pixel) in row_buffer.iter().enumerate() {
            if let Some((low, high)) = src_key {
                if (low..=high).contains(pixel) {
                    continue;
                }
            }
            if let Some((low, high)) = dest_key {
                if (low..=high).contains(&composed[column]) {
                    continue;
                }
            }
            composed[column] = *pixel;
        }
        call.memory.write_bytes(destination, &composed)?;
    }
    host.ddraw.row = row_buffer;
    host.ddraw.composed = composed;
    Ok(Return(DD_OK))
}

/// Paints one palette index across a rectangle, which is `DDBLT_COLORFILL`.
fn fill(
    host: &mut Host,
    call: &mut Call<'_>,
    this: u32,
    rect: u32,
    colour: u8,
) -> Result<Action> {
    let Some(surface) = host.ddraw.surfaces.get(&this) else {
        return Err(format!("DirectDraw: fill of {this:#010x}, which is not a surface"));
    };
    let (left, top, right, bottom) = bounds(call, rect, surface)?;
    let row = vec![colour; (right - left) as usize];
    for y in top as u32..bottom as u32 {
        let at = surface.buffer + y * surface.pitch + left as u32;
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
    fn a_depth_other_than_eight_is_refused() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let args = STACK_BASE + 0xcf00;
        memory.write_u32(args, 0x1234).unwrap();
        memory.write_u32(args + 4, 640).unwrap();
        memory.write_u32(args + 8, 480).unwrap();
        memory.write_u32(args + 12, 16).unwrap();
        let mut call = Call { memory: &mut memory, args, arg_bytes: 16 };
        let error = serve(&mut host, &mut call, thunks::DDRAW, "SetDisplayMode").unwrap_err();
        assert!(error.contains("16 bits per pixel"), "{error}");
    }
}
