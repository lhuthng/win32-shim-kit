//! The `GDIPLUS.dll` subset a game loads its artwork through.
//!
//! A program that ships its art as PNG files does not get pixels from GDI: it
//! asks GDI+ for a bitmap, reads the format and the palette, locks the pixels and
//! copies them into a surface of its own, then throws the bitmap away. That is the
//! whole of this table -- an image object whose pixels live on the host side, the
//! lock that hands them to the guest in the format it asked for, and the palette
//! that says what an indexed one means.
//!
//! The decoding is [`crate::image`]'s; this module is the boundary. An image is a
//! guest-visible handle and nothing else: the guest never dereferences it, so it
//! can be a number above GDI's own handles.
//!
//! A lock allocates the buffer out of the arena and converts into it, because the
//! guest reads the pixels through a pointer, and `GdipBitmapUnlockBits` gives the
//! block back -- with the guest's changes folded into the image when it asked for
//! a writable lock. Nothing is kept alive between locks, so a program that loads
//! several hundred sprites one after another holds one buffer at a time.

use std::collections::HashMap;

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const GDIPLUS: &str = "GDIPLUS.dll";

/// Image handles, above GDI's bitmaps and contexts so a log line can say which
/// object a number names.
const FIRST_IMAGE: u32 = 0x0005_0000;

/// `Status`: the two answers a working call gives.
const OK: u32 = 0;
const INVALID_PARAMETER: u32 = 2;
const OBJECT_BUSY: u32 = 4;
const FILE_NOT_FOUND: u32 = 10;
const UNKNOWN_IMAGE_FORMAT: u32 = 13;

/// `PixelFormat`: the layout of a locked buffer, which is what the guest reads.
const FORMAT_1BPP_INDEXED: u32 = 0x0003_0101;
const FORMAT_4BPP_INDEXED: u32 = 0x0003_0402;
const FORMAT_8BPP_INDEXED: u32 = 0x0003_0803;
const FORMAT_16BPP_RGB565: u32 = 0x0002_1005;
const FORMAT_24BPP_RGB: u32 = 0x0002_1808;
const FORMAT_32BPP_RGB: u32 = 0x0002_2009;
const FORMAT_32BPP_ARGB: u32 = 0x0026_200a;
const FORMAT_32BPP_PARGB: u32 = 0x000e_200b;

/// `ImageLockModeWrite`: the guest will change the pixels and wants them back.
const LOCK_WRITE: u32 = 2;

/// `BITMAPINFOHEADER::biCompression`: how a bitmap's pixels are stored.
const BI_RLE8: u32 = 1;
const BI_RLE4: u32 = 2;
const BI_BITFIELDS: u32 = 3;
const BI_ALPHABITFIELDS: u32 = 6;

pub const SHIMS: &[Binding] = &[
    Binding { dll: GDIPLUS, name: "GdiplusStartup", arg_bytes: 12, shim: startup },
    Binding { dll: GDIPLUS, name: "GdipCreateBitmapFromFile", arg_bytes: 8, shim: create_bitmap_from_file },
    Binding { dll: GDIPLUS, name: "GdipCreateBitmapFromResource", arg_bytes: 12, shim: create_bitmap_from_resource },
    Binding { dll: GDIPLUS, name: "GdipGetImageWidth", arg_bytes: 8, shim: get_image_width },
    Binding { dll: GDIPLUS, name: "GdipGetImageHeight", arg_bytes: 8, shim: get_image_height },
    Binding { dll: GDIPLUS, name: "GdipGetImagePixelFormat", arg_bytes: 8, shim: get_image_pixel_format },
    Binding { dll: GDIPLUS, name: "GdipBitmapLockBits", arg_bytes: 20, shim: lock_bits },
    Binding { dll: GDIPLUS, name: "GdipBitmapUnlockBits", arg_bytes: 8, shim: unlock_bits },
    Binding { dll: GDIPLUS, name: "GdipGetImagePaletteSize", arg_bytes: 8, shim: get_image_palette_size },
    Binding { dll: GDIPLUS, name: "GdipGetImagePalette", arg_bytes: 12, shim: get_image_palette },
    Binding { dll: GDIPLUS, name: "GdipSetImagePalette", arg_bytes: 8, shim: set_image_palette },
    Binding { dll: GDIPLUS, name: "GdipDisposeImage", arg_bytes: 4, shim: dispose_image },
];

/// Whether the image trace is on. A game loads hundreds of images at start-up, so
/// the switch is read once rather than on every call.
static LOGGING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

/// A bitmap this module handed out: its pixels, its palette, and the lock that is
/// open on it, if one is.
struct Image {
    width: u32,
    height: u32,
    /// The `PixelFormat` the file itself is in, which is what a guest reads back
    /// before deciding how to convert it.
    format: u32,
    /// `ARGB` entries, empty when the pixels carry their own colour.
    palette: Vec<u32>,
    /// The palette index of each pixel, top row first, when the image is
    /// palettised: an indexed bitmap is its indices coloured through its table,
    /// so replacing the table has to recolour the pixels. Empty for an image
    /// whose pixels carry their own colour.
    indices: Vec<u8>,
    /// RGBA, four bytes a pixel, top row first.
    pixels: Vec<u8>,
    locked: Option<Locked>,
}

/// A block of guest memory holding a region of an image in the format the guest
/// asked for, while the lock is open.
struct Locked {
    buffer: u32,
    stride: u32,
    /// Where the region starts in the image, and how large it is.
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    format: u32,
    /// Whether the guest asked to write into it.
    write: bool,
}

/// The images a session has loaded, by handle.
#[derive(Default)]
pub struct Gdiplus {
    images: HashMap<u32, Image>,
    next_image: u32,
}

/// `GdiplusStartup(&token, &input, &output)`: GDI+ is always started here, so the
/// token is a constant and the output structure -- the GDI+ hooks a caller may
/// supply -- is empty because this host calls no notification.
fn startup(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let token = call.arg(0)?;
    let output = call.arg(2)?;
    if token != 0 {
        call.write_u32(token, 1)?;
    }
    if output != 0 {
        // `GdiplusStartupOutput` is two pointer-sized fields on 32-bit.
        call.zero(output, 8)?;
    }
    Ok(Return(OK))
}

/// `GdipCreateBitmapFromFile(name, &bitmap)`: the PNG at that path, decoded.
///
/// The name is a wide path in the guest's own spelling, so it resolves through
/// the same file table `CreateFileA` reads -- which matches on the path's own
/// case-insensitive form and falls back to the basename, since the bundle's keys
/// are the archive's names rather than the guest's.
fn create_bitmap_from_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(0)?;
    let destination = call.arg(1)?;
    if name == 0 || destination == 0 {
        return Ok(Return(INVALID_PARAMETER));
    }
    let path = call.wstr(name)?;
    let Some(bytes) = host.files.contents(&path) else {
        host.notice(format!("GdipCreateBitmapFromFile: {path} is not in the bundle"));
        return Ok(Return(FILE_NOT_FOUND));
    };
    let bytes = bytes.to_vec();
    match decode(&bytes) {
        Ok(image) => {
            if crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI") {
                host.notice(format!(
                    "GdipCreateBitmapFromFile: {path} {}x{} format {:#x} colours {}",
                    image.width, image.height, image.format, image.palette.len()
                ));
            }
            let handle = host.gdiplus.insert(image);
            call.write_u32(destination, handle)?;
            Ok(Return(OK))
        }
        Err(error) => {
            host.notice(format!("GdipCreateBitmapFromFile: {path}: {error}"));
            Ok(Return(UNKNOWN_IMAGE_FORMAT))
        }
    }
}

/// `GdipCreateBitmapFromResource(instance, name, &bitmap)`: an image compiled into
/// the executable, addressed by name or by ordinal.
///
/// The name is a string, or an integer small enough to be `MAKEINTRESOURCE` --
/// which is how a resource named by a number arrives, since it is not a pointer at
/// all. Either way the bytes are the resource's own, and the decoder is the same
/// one a file goes through because GDI+ sniffs the format rather than trusting the
/// extension.
fn create_bitmap_from_resource(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(1)?;
    let destination = call.arg(2)?;
    if name == 0 || destination == 0 {
        return Ok(Return(INVALID_PARAMETER));
    }
    let resource = if name < 0x1_0000 {
        host.resources
            .iter()
            .find(|resource| {
                matches!(resource.name, crate::pe::ResourceKey::Id(id) if id == name)
                    || matches!(&resource.name, crate::pe::ResourceKey::Name(text) if text == &format!("#{name}"))
            })
            .map(|resource| (resource.rva, resource.size))
    } else {
        let wanted = call.wstr(name)?;
        host.resources
            .iter()
            .find(|resource| {
                matches!(&resource.name, crate::pe::ResourceKey::Name(text) if text.eq_ignore_ascii_case(&wanted))
                    || matches!(resource.name, crate::pe::ResourceKey::Id(id) if wanted == format!("#{id}"))
            })
            .map(|resource| (resource.rva, resource.size))
    };
    let Some((rva, size)) = resource else {
        // A resource that is not there is the normal case for a program that
        // keeps some of its art in the executable and the rest on disk: it asks
        // for the resource first and reads the file when that fails. The failure
        // is the answer, and saying so on every miss would bury the notices that
        // matter.
        return Ok(Return(INVALID_PARAMETER));
    };
    // The resource table names the data by RVA; the image is mapped at its base,
    // so the bytes are read from where `LoadResource` would have pointed.
    let bytes = call
        .memory
        .read_bytes(crate::pe::IMAGE_BASE.wrapping_add(rva), size as usize)?
        .to_vec();
    match decode(&bytes) {
        Ok(image) => {
            let handle = host.gdiplus.insert(image);
            call.write_u32(destination, handle)?;
            Ok(Return(OK))
        }
        Err(error) => {
            host.notice(format!("GdipCreateBitmapFromResource {name:#x}: {error}"));
            Ok(Return(UNKNOWN_IMAGE_FORMAT))
        }
    }
}

/// The image a call names, or `None` for a handle this host never issued.
impl Gdiplus {
    fn insert(&mut self, image: Image) -> u32 {
        let handle = FIRST_IMAGE + self.next_image * 4;
        self.next_image += 1;
        self.images.insert(handle, image);
        handle
    }
}

fn get_image_width(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(image) = host.gdiplus.images.get(&call.arg(0)?) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    let width = image.width;
    call.write_u32(call.arg(1)?, width)?;
    Ok(Return(OK))
}

fn get_image_height(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(image) = host.gdiplus.images.get(&call.arg(0)?) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    let height = image.height;
    call.write_u32(call.arg(1)?, height)?;
    Ok(Return(OK))
}

fn get_image_pixel_format(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(image) = host.gdiplus.images.get(&call.arg(0)?) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    let format = image.format;
    call.write_u32(call.arg(1)?, format)?;
    Ok(Return(OK))
}

/// `GdipBitmapLockBits(bitmap, rect, flags, format, data)`: a guest-visible copy
/// of a region's pixels.
///
/// `rect` is the region, or null for the whole image; `format` is the layout the
/// guest wants to read, or zero for "whatever this host has"; `flags` only matters
/// for its write bit, which makes the unlock fold the guest's pixels back in.
///
/// The buffer is a block of the arena, so the guest reads it with its own loads,
/// and the `BitmapData` it is described by is a 24-byte structure: size, stride,
/// format, pointer.
fn lock_bits(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let rect = call.arg(1)?;
    let flags = call.arg(2)?;
    let requested = call.arg(3)?;
    let data = call.arg(4)?;
    if data == 0 {
        return Ok(Return(INVALID_PARAMETER));
    }
    let Some((image_width, image_height, busy)) = host
        .gdiplus
        .images
        .get(&handle)
        .map(|image| (image.width, image.height, image.locked.is_some()))
    else {
        return Ok(Return(INVALID_PARAMETER));
    };
    if busy {
        // GDI+ refuses a second lock on the same bitmap rather than handing out
        // two buffers that both claim to be the pixels.
        return Ok(Return(OBJECT_BUSY));
    }
    let (x, y, width, height) = if rect == 0 {
        (0, 0, image_width, image_height)
    } else {
        (
            call.memory.read_u32(rect)?,
            call.memory.read_u32(rect + 4)?,
            call.memory.read_u32(rect + 8)?,
            call.memory.read_u32(rect + 12)?,
        )
    };
    if width == 0
        || height == 0
        || x.saturating_add(width) > image_width
        || y.saturating_add(height) > image_height
    {
        host.notice(format!(
            "GdipBitmapLockBits: {x},{y} {width}x{height} is outside a {image_width}x{image_height} image"
        ));
        return Ok(Return(INVALID_PARAMETER));
    }
    let format = if requested == 0 { FORMAT_32BPP_ARGB } else { requested };
    if std::env::var("SHIMKIT_LOG_GDI").is_ok() {
        let alpha = host
            .gdiplus
            .images
            .get(&handle)
            .map(|image| pixel_at(image, 0, 0).3)
            .unwrap_or(0);
        let head = {
            let image = host.gdiplus.images.get(&handle).expect("the image is there");
            locked_pixels(image, 0, 0, 4, 1, format)
        };
        host.notice(format!(
            "GdipBitmapLockBits {image_width}x{image_height} format {format:#010x} pixel0 alpha {alpha} head {head:02x?}"
        ));
    }
    if !matches!(
        format,
        FORMAT_8BPP_INDEXED
            | FORMAT_16BPP_RGB565
            | FORMAT_24BPP_RGB
            | FORMAT_32BPP_RGB
            | FORMAT_32BPP_ARGB
            | FORMAT_32BPP_PARGB
    ) {
        host.notice(format!(
            "GdipBitmapLockBits: pixel format {format:#010x} is not one this host converts to"
        ));
        return Ok(Return(INVALID_PARAMETER));
    }
    let stride = stride_of(width, format);
    let size = stride * height;
    let pixels = {
        let image = host.gdiplus.images.get(&handle).expect("the image is there");
        locked_pixels(image, x, y, width, height, format)
    };
    let buffer = host.allocate(size.max(4), 4)?;
    call.write(buffer, &pixels)?;
    call.write_u32(data, width)?;
    call.write_u32(data + 4, height)?;
    call.write_u32(data + 8, stride)?;
    call.write_u32(data + 12, format)?;
    call.write_u32(data + 16, buffer)?;
    call.write_u32(data + 20, 0)?;
    host.gdiplus.images.get_mut(&handle).expect("the image is there").locked = Some(Locked {
        buffer,
        stride,
        x,
        y,
        width,
        height,
        format,
        write: flags & LOCK_WRITE != 0,
    });
    Ok(Return(OK))
}

/// `GdipBitmapUnlockBits(bitmap, data)`: the end of the lock, and the guest's
/// pixels folded back in when it asked to write.
fn unlock_bits(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let Some(locked) = host
        .gdiplus
        .images
        .get_mut(&handle)
        .and_then(|image| image.locked.take())
    else {
        return Ok(Return(INVALID_PARAMETER));
    };
    if locked.write {
        let bytes = call
            .memory
            .read_bytes(locked.buffer, (locked.stride * locked.height) as usize)?
            .to_vec();
        if let Some(image) = host.gdiplus.images.get_mut(&handle) {
            store_locked_pixels(image, &locked, &bytes);
        }
    }
    host.release(locked.buffer);
    Ok(Return(OK))
}

/// `GdipGetImagePaletteSize(image, &size)`: the bytes a `ColorPalette` needs.
///
/// The structure is a flags word, a count, and one `ARGB` per entry, so an image
/// with no palette needs only the two words.
fn get_image_palette_size(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(image) = host.gdiplus.images.get(&call.arg(0)?) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    let size = 8 + image.palette.len() as u32 * 4;
    call.write_u32(call.arg(1)?, size)?;
    Ok(Return(OK))
}

/// `GdipGetImagePalette(image, palette, size)`: the colour table, copied out.
fn get_image_palette(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(1)?;
    let size = call.arg(2)?;
    let Some(image) = host.gdiplus.images.get(&call.arg(0)?) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    let needed = 8 + image.palette.len() as u32 * 4;
    if destination == 0 || size < needed {
        return Ok(Return(INVALID_PARAMETER));
    }
    call.write_u32(destination, 0)?;
    call.write_u32(destination + 4, image.palette.len() as u32)?;
    let mut entries = Vec::with_capacity(image.palette.len() * 4);
    for entry in &image.palette {
        entries.extend_from_slice(&entry.to_le_bytes());
    }
    call.write(destination + 8, &entries)?;
    Ok(Return(OK))
}

/// `GdipSetImagePalette(image, palette)`: replaces the colour table, which is what
/// a program that recolours indexed artwork does before locking it.
fn set_image_palette(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let palette = call.arg(1)?;
    if palette == 0 {
        return Ok(Return(INVALID_PARAMETER));
    }
    let Some(image) = host.gdiplus.images.get_mut(&handle) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    let count = call.memory.read_u32(palette + 4)? as usize;
    // A count larger than the table has room for is a caller that never read the
    // size, and GDI+ rejects it rather than reading past the structure.
    if count > 256 {
        return Ok(Return(INVALID_PARAMETER));
    }
    let mut entries = Vec::with_capacity(count);
    for index in 0..count as u32 {
        entries.push(call.memory.read_u32(palette + 8 + index * 4)?);
    }
    // A table with no alpha at all is a plain bitmap colour table, whose alpha
    // byte is reserved: it leaves the picture opaque rather than blanking it.
    let alpha = entries.iter().any(|entry| entry >> 24 != 0);
    let trace = format!(
        "SetImagePalette {handle:#x} {}x{} format {:#x} indices {} count {count} alpha {alpha} entries {:?}",
        image.width, image.height, image.format, image.indices.len(),
        &entries[..entries.len().min(6)]
    );
    let logging = crate::shim::switch(&LOGGING, "SHIMKIT_LOG_GDI");
    image.palette = entries;
    // An indexed image is its indices coloured through its table, so replacing the
    // table recolours the pixels. A program makes a colour key this way: it
    // rewrites the key entry to transparent and then locks the pixels, expecting
    // the change to be in them.
    if !image.indices.is_empty() {
        for (at, index) in image.indices.iter().enumerate() {
            let entry = image
                .palette
                .get(usize::from(*index))
                .copied()
                .unwrap_or(0xff00_0000);
            let pixel = &mut image.pixels[at * 4..at * 4 + 4];
            pixel[0] = (entry >> 16) as u8;
            pixel[1] = (entry >> 8) as u8;
            pixel[2] = entry as u8;
            pixel[3] = if alpha { (entry >> 24) as u8 } else { 0xff };
        }
    }
    if logging {
        host.notice(trace);
    }
    Ok(Return(OK))
}

/// `GdipDisposeImage(image)`: forgets the image and gives back a lock that was
/// never closed, because a program that disposes a locked bitmap expects the
/// memory to go with it.
fn dispose_image(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(image) = host.gdiplus.images.remove(&call.arg(0)?) else {
        return Ok(Return(INVALID_PARAMETER));
    };
    if let Some(locked) = image.locked {
        host.release(locked.buffer);
    }
    Ok(Return(OK))
}

/// The bytes of an image file or resource, as an image.
///
/// GDI+ sniffs the format rather than trusting an extension, and so does this:
/// a PNG is the file form, a `.bmp` file starts with its file header, and a
/// `BITMAPINFOHEADER` with no file header at all is a bitmap resource compiled
/// into an executable -- which is how this game ships the art its menu needs.
fn decode(bytes: &[u8]) -> std::result::Result<Image, String> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return from_png(bytes);
    }
    if bytes.starts_with(b"BM") {
        // A bitmap file says where its pixels are, which is not always right after
        // the header: a bitfield one puts its colour masks in between.
        let at = u32::from_le_bytes([bytes[10], bytes[11], bytes[12], bytes[13]]) as usize;
        return from_dib(&bytes[14..], at.checked_sub(14));
    }
    if bytes.len() >= 40 {
        return from_dib(bytes, None);
    }
    Err("the bytes are not a PNG, a bitmap file or a bitmap resource".to_string())
}

/// A PNG's pixels, with the format its colour type maps to.
fn from_png(bytes: &[u8]) -> std::result::Result<Image, String> {
    let decoded = crate::image::decode_png(bytes)?;
    let format = match decoded.colour_type {
        3 => match decoded.bit_depth {
            1 => FORMAT_1BPP_INDEXED,
            2 | 4 => FORMAT_4BPP_INDEXED,
            _ => FORMAT_8BPP_INDEXED,
        },
        2 => FORMAT_24BPP_RGB,
        // Grey is expanded to colour, so the guest reads the same three channels
        // whatever the file stored.
        _ => if std::env::var("SHIMKIT_PARGB").is_ok() {
            FORMAT_32BPP_PARGB
        } else {
            FORMAT_32BPP_ARGB
        },
    };
    // A palettised file stores no indices, only colours, so the indices are
    // recovered by matching each pixel against the table the file shipped with.
    let indices = if matches!(format, FORMAT_1BPP_INDEXED | FORMAT_4BPP_INDEXED | FORMAT_8BPP_INDEXED)
    {
        index_pixels(&decoded.pixels, &decoded.palette)
    } else {
        Vec::new()
    };
    Ok(Image {
        width: decoded.width,
        height: decoded.height,
        format,
        palette: decoded.palette,
        indices,
        pixels: decoded.pixels,
        locked: None,
    })
}

/// A `BITMAPINFOHEADER`, its colour table and its pixels, as an image.
///
/// The header is the `BITMAPINFO` a `CreateDIBSection` is handed, so the fields
/// read exactly as they do there: a negative height stores its rows top-down, a
/// palettised depth carries one `BGRA` entry per colour, and the pixels follow the
/// table, each row padded out to four bytes.
fn from_dib(bytes: &[u8], pixels_at: Option<usize>) -> std::result::Result<Image, String> {
    let header = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
    if header < 40 || bytes.len() < header {
        return Err(format!("a bitmap resource with a {header}-byte header"));
    }
    let read = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let width = read(4) as i32;
    let height = read(8) as i32;
    let planes = u16::from_le_bytes([bytes[12], bytes[13]]);
    let bits = u16::from_le_bytes([bytes[14], bytes[15]]);
    let compression = read(16);
    if width <= 0 || height == 0 {
        return Err(format!("a bitmap resource that is {width}x{height}"));
    }
    if planes != 1 {
        return Err(format!("a bitmap resource with {planes} planes"));
    }
    if !matches!(bits, 1 | 4 | 8 | 16 | 24 | 32) {
        return Err(format!("a {bits}-bit bitmap resource"));
    }
    // `BI_RGB`, `BI_RLE8`, `BI_RLE4` and `BI_BITFIELDS`: the layouts such a file
    // or resource carries. A run-length bitmap is always palettised, so its depth
    // says which of the two codes it uses.
    match compression {
        0 => {}
        BI_RLE8 if bits == 8 => {}
        BI_RLE4 if bits == 4 => {}
        BI_BITFIELDS | BI_ALPHABITFIELDS if bits == 16 || bits == 32 => {}
        other => return Err(format!("a {bits}-bit bitmap with compression {other}")),
    }
    // A bitfield bitmap names its channels' positions in the word rather than
    // assuming `BGRA`. The masks sit just past a plain `BITMAPINFOHEADER` and
    // inside a longer one, at the same distance either way.
    let bitfields = compression == BI_BITFIELDS || compression == BI_ALPHABITFIELDS;
    let masks = if bitfields {
        let channel = |index: usize| read(40 + index * 4);
        Some((
            channel(0),
            channel(1),
            channel(2),
            if compression == BI_ALPHABITFIELDS {
                channel(3)
            } else {
                0
            },
        ))
    } else {
        None
    };
    let palette_entries = if bits <= 8 {
        let used = read(32) as usize;
        if used == 0 {
            1 << bits
        } else {
            used.min(1 << bits)
        }
    } else {
        0
    };
    let table = &bytes[header..];
    let mut palette = Vec::with_capacity(palette_entries);
    for index in 0..palette_entries {
        let at = index * 4;
        if at + 3 >= table.len() {
            return Err("a bitmap resource whose colour table is short".to_string());
        }
        palette.push(
            0xff00_0000
                | (u32::from(table[at + 2]) << 16)
                | (u32::from(table[at + 1]) << 8)
                | u32::from(table[at]),
        );
    }
    let mask_bytes = match compression {
        BI_ALPHABITFIELDS => 16,
        BI_BITFIELDS => 12,
        _ => 0,
    };
    let pixels_at = pixels_at.unwrap_or(header + palette_entries * 4 + mask_bytes);
    let columns = width as usize;
    let rows = height.unsigned_abs() as usize;
    let bottom_up = height > 0;

    if bits <= 8 {
        let bands = dib_indices(bytes, pixels_at, columns, rows, bits, compression)?;
        let mut indices = Vec::with_capacity(columns * rows);
        let mut pixels = Vec::with_capacity(columns * rows * 4);
        for row in 0..rows {
            // The first band in the data is the bottom row of the picture unless
            // the header's height was negative.
            let source_row = if bottom_up { rows - 1 - row } else { row };
            for column in 0..columns {
                let index = bands[source_row * columns + column];
                indices.push(index);
                let entry = palette.get(usize::from(index)).copied().unwrap_or(0xff00_0000);
                pixels.extend_from_slice(&[
                    (entry >> 16) as u8,
                    (entry >> 8) as u8,
                    entry as u8,
                    (entry >> 24) as u8,
                ]);
            }
        }
        return Ok(Image {
            width: width as u32,
            height: rows as u32,
            // The depth the bitmap itself is, which is what a guest that asks is
            // answered with: a four-bit menu resource is a four-bit bitmap.
            format: match bits {
                1 => FORMAT_1BPP_INDEXED,
                4 => FORMAT_4BPP_INDEXED,
                _ => FORMAT_8BPP_INDEXED,
            },
            palette,
            indices,
            pixels,
            locked: None,
        });
    }

    let stride = ((width as u32 * u32::from(bits) + 31) / 32 * 4) as usize;
    if pixels_at + stride * rows > bytes.len() {
        return Err(format!(
            "a {}x{} bitmap resource whose pixels are {} bytes short",
            width,
            height,
            pixels_at + stride * rows - bytes.len()
        ));
    }
    let mut pixels = Vec::with_capacity(columns * rows * 4);
    for row in 0..rows {
        let source_row = if bottom_up { rows - 1 - row } else { row };
        let line = &bytes[pixels_at + source_row * stride..pixels_at + (source_row + 1) * stride];
        for column in 0..columns {
            if let Some((red_mask, green_mask, blue_mask, alpha_mask)) = masks {
                let value = if bits == 16 {
                    u32::from(u16::from_le_bytes([line[column * 2], line[column * 2 + 1]]))
                } else {
                    u32::from_le_bytes([
                        line[column * 4],
                        line[column * 4 + 1],
                        line[column * 4 + 2],
                        line[column * 4 + 3],
                    ])
                };
                let alpha = if alpha_mask != 0 {
                    channel_of(value, alpha_mask)
                } else {
                    0xff
                };
                pixels.extend_from_slice(&[
                    channel_of(value, red_mask),
                    channel_of(value, green_mask),
                    channel_of(value, blue_mask),
                    alpha,
                ]);
                continue;
            }
            let (red, green, blue) = match bits {
                16 => {
                    let value = u16::from_le_bytes([line[column * 2], line[column * 2 + 1]]);
                    (
                        (((value >> 11) & 0x1f) as u8) << 3,
                        (((value >> 5) & 0x3f) as u8) << 2,
                        ((value & 0x1f) as u8) << 3,
                    )
                }
                24 => (
                    line[column * 3 + 2],
                    line[column * 3 + 1],
                    line[column * 3],
                ),
                _ => (
                    line[column * 4 + 2],
                    line[column * 4 + 1],
                    line[column * 4],
                ),
            };
            // A resource's fourth byte is reserved on a `BI_RGB` bitmap, so it is
            // treated as opaque rather than as an alpha channel.
            pixels.extend_from_slice(&[red, green, blue, 0xff]);
        }
    }
    let format = match bits {
        16 => FORMAT_16BPP_RGB565,
        24 => FORMAT_24BPP_RGB,
        _ => FORMAT_32BPP_ARGB,
    };
    Ok(Image {
        width: width as u32,
        height: rows as u32,
        format,
        palette,
        indices: Vec::new(),
        pixels,
        locked: None,
    })
}

/// The palette index of every pixel, recovered by matching each pixel's colour
/// against the table a palettised file shipped with.
fn index_pixels(pixels: &[u8], palette: &[u32]) -> Vec<u8> {
    pixels
        .chunks_exact(4)
        .map(|pixel| {
            palette
                .iter()
                .position(|entry| {
                    (entry >> 16) as u8 == pixel[0]
                        && (entry >> 8) as u8 == pixel[1]
                        && *entry as u8 == pixel[2]
                })
                .map_or(0, |index| index as u8)
        })
        .collect()
}

/// One channel of a bitfield pixel, scaled from however many bits its mask holds
/// to the eight a colour channel is.
fn channel_of(value: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let width = mask.count_ones();
    let raw = (value & mask) >> shift;
    let max = (1_u32 << width) - 1;
    (raw * 255 / max) as u8
}

/// The palette indices of a palettised bitmap, in the order its rows are stored:
/// the first band is the bottom row of the picture unless the height was negative.
///
/// `BI_RGB` stores them packed, most significant bits first; `BI_RLE8` and
/// `BI_RLE4` store them as a stream of runs, which is how a menu's large flat
/// areas stay small enough to compile into the executable.
fn dib_indices(
    bytes: &[u8],
    pixels_at: usize,
    columns: usize,
    rows: usize,
    bits: u16,
    compression: u32,
) -> std::result::Result<Vec<u8>, String> {
    let mut indices = vec![0_u8; columns * rows];
    match compression {
        BI_RLE8 => {
            let stream = &bytes[pixels_at.min(bytes.len())..];
            run_length(stream, &mut indices, columns, rows, false)?;
        }
        BI_RLE4 => {
            let stream = &bytes[pixels_at.min(bytes.len())..];
            run_length(stream, &mut indices, columns, rows, true)?;
        }
        _ => {
            let stride = ((columns as u32 * u32::from(bits) + 31) / 32 * 4) as usize;
            if pixels_at + stride * rows > bytes.len() {
                return Err(format!(
                    "a {columns}x{rows} bitmap resource whose pixels are {} bytes short",
                    pixels_at + stride * rows - bytes.len()
                ));
            }
            for band in 0..rows {
                let line = &bytes[pixels_at + band * stride..pixels_at + (band + 1) * stride];
                for column in 0..columns {
                    indices[band * columns + column] = match bits {
                        8 => line[column],
                        4 => (line[column / 2] >> if column % 2 == 0 { 4 } else { 0 }) & 0x0f,
                        _ => (line[column / 8] >> (7 - column % 8)) & 0x01,
                    };
                }
            }
        }
    }
    Ok(indices)
}

/// `BI_RLE8` and `BI_RLE4`: runs of one colour, literal runs, and skips.
///
/// A run is a count and a value, two bytes. A count of zero introduces an escape
/// in its place: the escape *is* the pair's second byte, so it is read from there
/// rather than from the next pair. Zero means end of line, one end of bitmap, two
/// a delta to the next run's position, and three or more a literal run whose
/// pixels follow, padded out to a word. A nibble bitmap's runs alternate between
/// the two halves of the run's colour byte, and its literals pack two pixels into
/// each byte.
fn run_length(
    stream: &[u8],
    indices: &mut [u8],
    columns: usize,
    rows: usize,
    nibbles: bool,
) -> std::result::Result<(), String> {
    let mut position = 0;
    let (mut x, mut y) = (0_usize, 0_usize);
    let put = |indices: &mut [u8], x: usize, y: usize, value: u8| {
        if y < rows && x < columns {
            indices[y * columns + x] = value;
        }
    };
    while position + 1 < stream.len() && y < rows {
        let count = usize::from(stream[position]);
        if count > 0 {
            let value = stream[position + 1];
            position += 2;
            for index in 0..count {
                let pixel = if nibbles {
                    if index % 2 == 0 {
                        value >> 4
                    } else {
                        value & 0x0f
                    }
                } else {
                    value
                };
                put(indices, x, y, pixel);
                x += 1;
            }
            continue;
        }
        let escape = usize::from(stream[position + 1]);
        position += 2;
        match escape {
            0 => {
                x = 0;
                y += 1;
            }
            1 => break,
            2 => {
                let (dx, dy) = (
                    usize::from(*stream.get(position).unwrap_or(&0)),
                    usize::from(*stream.get(position + 1).unwrap_or(&0)),
                );
                position += 2;
                x += dx;
                y += dy;
            }
            literal => {
                for index in 0..literal {
                    // Two pixels share a byte when they are nibbles, the first
                    // in the high half.
                    let at = position + if nibbles { index / 2 } else { index };
                    let byte = *stream.get(at).unwrap_or(&0);
                    let pixel = if nibbles {
                        if index % 2 == 0 {
                            byte >> 4
                        } else {
                            byte & 0x0f
                        }
                    } else {
                        byte
                    };
                    put(indices, x, y, pixel);
                    x += 1;
                }
                // The run is stored in bytes -- two pixels to a byte when they
                // are nibbles -- and the whole thing is padded to a word.
                let used = if nibbles { (literal + 1) / 2 } else { literal };
                position += used + used % 2;
            }
        }
    }
    Ok(())
}

/// The stride of a locked buffer: its rows packed out to four bytes.
fn stride_of(width: u32, format: u32) -> u32 {
    let bits = match format {
        FORMAT_8BPP_INDEXED => 8,
        FORMAT_16BPP_RGB565 => 16,
        FORMAT_24BPP_RGB => 24,
        _ => 32,
    };
    (width * bits + 31) / 32 * 4
}

/// One pixel of an image, as red, green, blue and alpha. Outside the image it is
/// transparent black, which is what a region that reaches past the edge reads.
fn pixel_at(image: &Image, x: u32, y: u32) -> (u8, u8, u8, u8) {
    if x >= image.width || y >= image.height {
        return (0, 0, 0, 0);
    }
    let at = ((y * image.width + x) * 4) as usize;
    (
        image.pixels[at],
        image.pixels[at + 1],
        image.pixels[at + 2],
        image.pixels[at + 3],
    )
}

/// The palette entry closest to a colour, which is how an indexed lock assigns a
/// pixel whose colour the table does not have -- GDI+ does the same rather than
/// failing.
fn nearest(palette: &[u32], colour: (u8, u8, u8)) -> u8 {
    let mut best = 0_usize;
    let mut distance = u32::MAX;
    for (index, entry) in palette.iter().enumerate() {
        let red = ((entry >> 16) & 0xff) as i32;
        let green = ((entry >> 8) & 0xff) as i32;
        let blue = (entry & 0xff) as i32;
        let delta = (red - i32::from(colour.0)).pow(2)
            + (green - i32::from(colour.1)).pow(2)
            + (blue - i32::from(colour.2)).pow(2);
        if (delta as u32) < distance {
            distance = delta as u32;
            best = index;
        }
    }
    best as u8
}

/// A region of an image, converted into the layout a lock asked for.
fn locked_pixels(image: &Image, x: u32, y: u32, width: u32, height: u32, format: u32) -> Vec<u8> {
    let stride = stride_of(width, format) as usize;
    let mut out = vec![0_u8; stride * height as usize];
    for row in 0..height {
        for column in 0..width {
            let (red, green, blue, alpha) = pixel_at(image, x + column, y + row);
            let at = row as usize * stride + column as usize * bytes_per_pixel(format);
            match format {
                FORMAT_8BPP_INDEXED => {
                    out[at] = nearest(&image.palette, (red, green, blue));
                }
                FORMAT_16BPP_RGB565 => {
                    let value = (u16::from(red) >> 3) << 11
                        | (u16::from(green) >> 2) << 5
                        | (u16::from(blue) >> 3);
                    out[at..at + 2].copy_from_slice(&value.to_le_bytes());
                }
                FORMAT_24BPP_RGB => {
                    out[at] = blue;
                    out[at + 1] = green;
                    out[at + 2] = red;
                }
                FORMAT_32BPP_RGB => {
                    out[at] = blue;
                    out[at + 1] = green;
                    out[at + 2] = red;
                    out[at + 3] = 0;
                }
                FORMAT_32BPP_PARGB => {
                    // Premultiplied: the colour is already multiplied by the
                    // alpha, which is the layout a blit adds without dividing.
                    let scale = |channel: u8| (u32::from(channel) * u32::from(alpha) / 255) as u8;
                    out[at] = scale(blue);
                    out[at + 1] = scale(green);
                    out[at + 2] = scale(red);
                    out[at + 3] = alpha;
                }
                _ => {
                    out[at] = blue;
                    out[at + 1] = green;
                    out[at + 2] = red;
                    out[at + 3] = alpha;
                }
            }
        }
    }
    out
}

/// A writable lock's pixels, folded back into the image they came from.
fn store_locked_pixels(image: &mut Image, locked: &Locked, bytes: &[u8]) {
    for row in 0..locked.height {
        for column in 0..locked.width {
            let at = row as usize * locked.stride as usize
                + column as usize * bytes_per_pixel(locked.format);
            let (red, green, blue, alpha) = match locked.format {
                FORMAT_8BPP_INDEXED => {
                    let entry = image
                        .palette
                        .get(usize::from(bytes[at]))
                        .copied()
                        .unwrap_or(0xff00_0000);
                    (
                        ((entry >> 16) & 0xff) as u8,
                        ((entry >> 8) & 0xff) as u8,
                        (entry & 0xff) as u8,
                        ((entry >> 24) & 0xff) as u8,
                    )
                }
                FORMAT_16BPP_RGB565 => {
                    let value = u16::from_le_bytes([bytes[at], bytes[at + 1]]);
                    (
                        ((value >> 11) as u8) << 3 | ((value >> 11) as u8 >> 2),
                        (((value >> 5) & 0x3f) as u8) << 2 | (((value >> 5) & 0x3f) as u8 >> 4),
                        ((value & 0x1f) as u8) << 3 | ((value & 0x1f) as u8 >> 2),
                        0xff,
                    )
                }
                FORMAT_24BPP_RGB => (bytes[at + 2], bytes[at + 1], bytes[at], 0xff),
                FORMAT_32BPP_RGB => (bytes[at + 2], bytes[at + 1], bytes[at], 0xff),
                _ => (bytes[at + 2], bytes[at + 1], bytes[at], bytes[at + 3]),
            };
            let x = locked.x + column;
            let y = locked.y + row;
            if x < image.width && y < image.height {
                let into = ((y * image.width + x) * 4) as usize;
                image.pixels[into] = red;
                image.pixels[into + 1] = green;
                image.pixels[into + 2] = blue;
                image.pixels[into + 3] = alpha;
                // An indexed lock names entries, so the indices are what the guest
                // wrote and must stay in step with the pixels they coloured.
                if locked.format == FORMAT_8BPP_INDEXED {
                    let index = (y * image.width + x) as usize;
                    if index < image.indices.len() {
                        image.indices[index] = bytes[at];
                    }
                }
            }
        }
    }
}

/// How wide one pixel is in a locked buffer.
fn bytes_per_pixel(format: u32) -> usize {
    match format {
        FORMAT_8BPP_INDEXED => 1,
        FORMAT_16BPP_RGB565 => 2,
        FORMAT_24BPP_RGB => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, HEAP_BASE, STACK_BASE};

    fn machine() -> (Host, GuestMemory) {
        let memory = GuestMemory::empty(HEAP_BASE as usize + 0x4000).expect("blank memory");
        (Host::new(), memory)
    }

    /// Runs one shim of this table with its arguments on the guest stack.
    fn call_shim(
        host: &mut Host,
        memory: &mut GuestMemory,
        name: &str,
        arguments: &[u32],
    ) -> Action {
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("{name} is implemented"));
        let args = STACK_BASE + 0x1000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).expect("the stack writes");
        let mut call = Call {
            memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call).expect("the shim answers")
    }

    /// A `BITMAPINFOHEADER`, a colour table of `colours`, and then `pixels`.
    fn dib(width: i32, height: i32, bits: u16, compression: u32, colours: &[u32], pixels: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&40_u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1_u16.to_le_bytes());
        out.extend_from_slice(&bits.to_le_bytes());
        out.extend_from_slice(&compression.to_le_bytes());
        out.extend_from_slice(&(pixels.len() as u32).to_le_bytes());
        out.extend_from_slice(&0_u32.to_le_bytes()); // x pixels per metre
        out.extend_from_slice(&0_u32.to_le_bytes()); // y pixels per metre
        out.extend_from_slice(&(colours.len() as u32).to_le_bytes()); // colours used
        out.extend_from_slice(&0_u32.to_le_bytes()); // colours important
        for colour in colours {
            out.extend_from_slice(&colour.to_le_bytes());
        }
        out.extend_from_slice(pixels);
        out
    }

    #[test]
    fn a_run_length_resource_is_expanded_bottom_row_first() {
        // Four by two, one bit a pixel: the first band is the bottom row.
        let bytes = dib(
            4,
            2,
            8,
            BI_RLE8,
            &[0x0000_0000, 0x00ff_0000],
            // Bottom row: four pixels of index 1, end of line. Top row: four of
            // index 0 in one literal run, then end of line and end of bitmap.
            &[4, 1, 0, 0, 0, 4, 0, 0, 0, 0, 0, 1],
        );
        let image = decode(&bytes).expect("the resource decodes");
        assert_eq!((image.width, image.height), (4, 2));
        assert_eq!(image.format, FORMAT_8BPP_INDEXED);
        assert_eq!(image.palette.len(), 2);
        // The top row is the black one, the bottom row the red one: pixels four at
        // a time, so the second row starts at the fourth pixel.
        assert_eq!(&image.pixels[0..4], &[0, 0, 0, 255]);
        assert_eq!(&image.pixels[16..20], &[255, 0, 0, 255]);
    }

    #[test]
    fn a_nibble_literal_packs_two_pixels_a_byte_and_its_escape_is_the_pairs_second_byte() {
        // Eight by two at four bits, which is the form the menu art is compiled
        // into: a run of eight of index 1, an end of line, then a literal of
        // eight -- two pixels a byte -- and the end of the bitmap.
        let colours: Vec<u32> = (0..9).map(|index| (index as u32) << 16).collect();
        let bytes = dib(
            8,
            2,
            4,
            BI_RLE4,
            &colours,
            &[8, 0x11, 0, 0, 0, 8, 0x12, 0x34, 0x56, 0x78, 0, 0, 0, 1],
        );
        let image = decode(&bytes).expect("the resource decodes");
        assert_eq!((image.width, image.height), (8, 2));
        // The literal is the top row, so it comes first in the image: its two
        // pixels a byte are the indices 1 to 8 in order.
        for index in 0..8 {
            let at = index * 4;
            assert_eq!(
                &image.pixels[at..at + 4],
                &[(index as u8) + 1, 0, 0, 255],
                "pixel {index} of the literal row"
            );
        }
        // The bottom row is the run of index 1, all eight pixels of it.
        for column in 0..8 {
            let at = 32 + column * 4;
            assert_eq!(&image.pixels[at..at + 4], &[1, 0, 0, 255]);
        }
    }

    #[test]
    fn a_bitmap_file_is_read_where_its_header_says_its_pixels_are() {
        // A 24-bit file: the file header, the DIB, then two pixels of blue and
        // one of red, each row padded to four bytes.
        let dib = dib(2, 1, 24, 0, &[], &[255, 0, 0, 0, 0, 255, 0, 0]);
        let mut file = b"BM".to_vec();
        // Where the pixels start: past the file header and the DIB header, there being
        // no colour table in a truecolour bitmap.
        file.extend_from_slice(&(14_u32 + 40).to_le_bytes());
        file.extend_from_slice(&0_u32.to_le_bytes());
        // Where the pixels start: past the file header and the DIB header, there being
        // no colour table in a truecolour bitmap.
        file.extend_from_slice(&(14_u32 + 40).to_le_bytes());
        file.extend_from_slice(&dib);
        let image = decode(&file).expect("the file decodes");
        assert_eq!(image.format, FORMAT_24BPP_RGB);
        assert_eq!(&image.pixels[0..4], &[0, 0, 255, 255]);
        assert_eq!(&image.pixels[4..8], &[255, 0, 0, 255]);
    }

    #[test]
    fn a_locked_bitmap_is_the_guests_pixels_in_the_format_it_asked_for() {
        let (mut host, mut memory) = machine();
        host.gdiplus.images.insert(
            FIRST_IMAGE,
            Image {
                width: 2,
                height: 1,
                format: FORMAT_24BPP_RGB,
                palette: Vec::new(),
                indices: Vec::new(),
                pixels: vec![10, 20, 30, 255, 40, 50, 60, 128],
                locked: None,
            },
        );
        let data = STACK_BASE + 0x2000;

        // A lock in 32bpp ARGB: `BitmapData` says how large the region is and
        // where its pixels are.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GdipBitmapLockBits",
                &[FIRST_IMAGE, 0, 0, FORMAT_32BPP_ARGB, data]
            ),
            Return(OK)
        );
        assert_eq!(memory.read_u32(data).unwrap(), 2, "the region's width");
        assert_eq!(memory.read_u32(data + 4).unwrap(), 1, "and its height");
        assert_eq!(memory.read_u32(data + 8).unwrap(), 8, "its stride");
        assert_eq!(memory.read_u32(data + 12).unwrap(), FORMAT_32BPP_ARGB);
        let pixels = memory.read_u32(data + 16).unwrap();
        assert_eq!(
            memory.read_bytes(pixels, 8).unwrap(),
            &[30, 20, 10, 255, 60, 50, 40, 128],
            "BGRA, which is what a 32bpp ARGB lock holds"
        );

        // The second lock of the same bitmap is refused rather than overwriting
        // the first one's buffer.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GdipBitmapLockBits",
                &[FIRST_IMAGE, 0, 0, FORMAT_32BPP_ARGB, data]
            ),
            Return(OBJECT_BUSY)
        );

        assert_eq!(
            call_shim(&mut host, &mut memory, "GdipBitmapUnlockBits", &[FIRST_IMAGE, data]),
            Return(OK)
        );
    }

    #[test]
    fn a_written_lock_folds_the_guests_pixels_back_into_the_image() {
        let (mut host, mut memory) = machine();
        host.gdiplus.images.insert(
            FIRST_IMAGE,
            Image {
                width: 1,
                height: 1,
                format: FORMAT_32BPP_ARGB,
                palette: Vec::new(),
                indices: Vec::new(),
                pixels: vec![1, 2, 3, 255],
                locked: None,
            },
        );
        let data = STACK_BASE + 0x2000;
        call_shim(
            &mut host,
            &mut memory,
            "GdipBitmapLockBits",
            &[FIRST_IMAGE, 0, LOCK_WRITE, FORMAT_32BPP_ARGB, data],
        );
        let pixels = memory.read_u32(data + 16).unwrap();
        memory.write_bytes(pixels, &[9, 8, 7, 255]).unwrap();
        call_shim(&mut host, &mut memory, "GdipBitmapUnlockBits", &[FIRST_IMAGE, data]);
        assert_eq!(
            host.gdiplus.images.get(&FIRST_IMAGE).unwrap().pixels,
            vec![7, 8, 9, 255]
        );
    }

    #[test]
    fn recolouring_an_indexed_images_table_recolours_its_pixels() {
        let (mut host, mut memory) = machine();
        host.gdiplus.images.insert(
            FIRST_IMAGE,
            Image {
                width: 2,
                height: 1,
                format: FORMAT_4BPP_INDEXED,
                palette: vec![0xff00_0000, 0xffff_ffff],
                indices: vec![0, 1],
                pixels: vec![0, 0, 0, 255, 255, 255, 255, 255],
                locked: None,
            },
        );
        // Entry zero is rewritten to transparent black, which is how a program
        // keys the black of its menu art out before locking the pixels.
        let palette = STACK_BASE + 0x2100;
        memory.write_u32(palette, 0).unwrap();
        memory.write_u32(palette + 4, 2).unwrap();
        memory.write_u32(palette + 8, 0x0000_0000).unwrap();
        memory.write_u32(palette + 12, 0xffff_ffff).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GdipSetImagePalette",
                &[FIRST_IMAGE, palette]
            ),
            Return(OK)
        );
        let image = host.gdiplus.images.get(&FIRST_IMAGE).unwrap();
        assert_eq!(
            image.pixels,
            vec![0, 0, 0, 0, 255, 255, 255, 255],
            "the keyed entry is now transparent and the other one is untouched"
        );
        assert_eq!(image.indices, vec![0, 1], "and the indices still name the entries");
    }

    #[test]
    fn an_indexed_image_reports_the_palette_it_was_loaded_with() {
        let (mut host, mut memory) = machine();
        host.gdiplus.images.insert(
            FIRST_IMAGE,
            Image {
                width: 1,
                height: 1,
                format: FORMAT_8BPP_INDEXED,
                palette: vec![0xff00_0000, 0xff00_00ff],
                indices: Vec::new(),
                pixels: vec![0, 0, 0, 255],
                locked: None,
            },
        );
        let size = STACK_BASE + 0x2000;
        call_shim(&mut host, &mut memory, "GdipGetImagePaletteSize", &[FIRST_IMAGE, size]);
        assert_eq!(memory.read_u32(size).unwrap(), 16, "two entries and two words");

        let palette = STACK_BASE + 0x2100;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GdipGetImagePalette",
                &[FIRST_IMAGE, palette, 16]
            ),
            Return(OK)
        );
        assert_eq!(memory.read_u32(palette + 4).unwrap(), 2, "the entry count");
        assert_eq!(memory.read_u32(palette + 12).unwrap(), 0xff00_00ff);
    }
}
