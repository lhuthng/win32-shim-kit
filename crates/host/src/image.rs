//! A PNG decoder, for the artwork a guest loads through GDI+.
//!
//! The engine has no dependencies, so this carries its own inflate: a game
//! rehosted here ships its images as PNG files, `GdipCreateBitmapFromFile`
//! decodes them, and the guest then reads the pixels back out with
//! `GdipBitmapLockBits`. Nothing draws through this module -- it produces the
//! pixels and the palette, and the shim copies them into the guest's buffer.
//!
//! What it decodes is what the archive holds: colour types 0, 2, 3, 4 and 6, bit
//! depths 1, 2, 4, 8 and 16, no interlacing. Pixels come out as RGBA8, top row
//! first, because that is the form the shims convert from; the palette and the
//! original colour type are kept so `GdipGetImagePixelFormat` can say what the
//! file itself is.

/// A decoded image.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    /// The PNG's own colour type: 0 grey, 2 RGB, 3 palette, 4 grey+alpha, 6 RGBA.
    pub colour_type: u8,
    /// The PNG's own bit depth: 1, 2, 4, 8 or 16.
    pub bit_depth: u8,
    /// `ARGB` entries, empty for an image whose pixels carry their own colour.
    pub palette: Vec<u32>,
    /// RGBA, four bytes a pixel, top row first.
    pub pixels: Vec<u8>,
}

impl Decoded {
    /// Pixels in a row, which is what a stride is computed from.
    pub fn stride(&self) -> u32 {
        self.width * 4
    }
}

const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];

/// The bytes of a PNG file, decoded.
pub fn decode_png(bytes: &[u8]) -> Result<Decoded, String> {
    if bytes.len() < 8 || bytes[..8] != SIGNATURE {
        return Err("not a PNG file".to_string());
    }
    let mut position = 8;
    let mut header: Option<(u32, u32, u8, u8, u8)> = None;
    let mut palette: Vec<u32> = Vec::new();
    let mut alphas: Vec<u8> = Vec::new();
    let mut compressed: Vec<u8> = Vec::new();
    while position + 8 <= bytes.len() {
        let length = u32::from_be_bytes([
            bytes[position],
            bytes[position + 1],
            bytes[position + 2],
            bytes[position + 3],
        ]) as usize;
        let kind = &bytes[position + 4..position + 8];
        let start = position + 8;
        let end = start
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| "a PNG chunk runs past the end of the file".to_string())?;
        let data = &bytes[start..end];
        position = end + 4;
        match kind {
            b"IHDR" => {
                if data.len() < 13 {
                    return Err("PNG header is short".to_string());
                }
                let width = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
                let height = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
                let (depth, colour, compression, filter, interlace) =
                    (data[8], data[9], data[10], data[11], data[12]);
                if compression != 0 {
                    return Err(format!("PNG uses compression method {compression}"));
                }
                if filter != 0 {
                    return Err(format!("PNG uses filter method {filter}"));
                }
                if interlace != 0 {
                    return Err("interlaced PNG: this host decodes the non-interlaced form".to_string());
                }
                if width == 0 || height == 0 {
                    return Err(format!("PNG is {width}x{height}"));
                }
                header = Some((width, height, depth, colour, interlace));
            }
            b"PLTE" => {
                palette = data
                    .chunks_exact(3)
                    .map(|entry| {
                        0xff00_0000
                            | (u32::from(entry[0]) << 16)
                            | (u32::from(entry[1]) << 8)
                            | u32::from(entry[2])
                    })
                    .collect();
            }
            b"tRNS" => alphas = data.to_vec(),
            b"IDAT" => compressed.extend_from_slice(data),
            b"IEND" => break,
            _ => {}
        }
    }
    let Some((width, height, depth, colour, _)) = header else {
        return Err("PNG has no header chunk".to_string());
    };
    if compressed.is_empty() {
        return Err("PNG has no image data".to_string());
    }
    let channels = match colour {
        0 => 1,
        2 => 3,
        3 => 1,
        4 => 2,
        6 => 4,
        other => return Err(format!("PNG colour type {other} is not one this host reads")),
    };
    if !matches!(depth, 1 | 2 | 4 | 8 | 16) {
        return Err(format!("PNG has a {depth}-bit sample, which is not a PNG depth"));
    }
    if colour != 3 && depth < 8 {
        return Err(format!(
            "PNG colour type {colour} at {depth} bits: only palette and grey images pack below a byte"
        ));
    }
    let bits_per_pixel = channels * u32::from(depth);
    let row_bytes = ((width as u64 * u64::from(bits_per_pixel) + 7) / 8) as usize;
    let raw_length = (row_bytes + 1)
        .checked_mul(height as usize)
        .ok_or_else(|| "PNG is too large".to_string())?;
    let raw = zlib(compressed.as_slice(), raw_length)?;
    if raw.len() < raw_length {
        return Err(format!(
            "PNG image data is {} bytes, {} short",
            raw.len(),
            raw_length - raw.len()
        ));
    }
    // The first row has no neighbour above it, so its filter only ever names
    // "none" or a filter that means the same on the first row.
    let mut rows: Vec<u8> = vec![0; row_bytes * height as usize];
    let filter_bytes = (bits_per_pixel + 7) / 8;
    for row in 0..height as usize {
        let source = &raw[row * (row_bytes + 1)..(row + 1) * (row_bytes + 1)];
        let filter = source[0];
        let line = &source[1..];
        // The row above is the one already decoded, so it is read out of the
        // finished part of the buffer while this row is written into the rest.
        let (finished, rest) = rows.split_at_mut(row * row_bytes);
        let target = &mut rest[..row_bytes];
        target.copy_from_slice(line);
        let prior = if row == 0 {
            None
        } else {
            Some(&finished[finished.len() - row_bytes..])
        };
        unfilter(filter, target, prior, filter_bytes as usize)?;
    }
    let pixels = expand(&rows, width, height, depth, colour, &palette, &alphas)?;
    Ok(Decoded {
        width,
        height,
        colour_type: colour,
        bit_depth: depth,
        palette: palette_with_alpha(palette, &alphas, depth),
        pixels,
    })
}

/// The palette as the entries a locked 8bpp image reports, with the transparency
/// chunk's alpha folded in.
fn palette_with_alpha(palette: Vec<u32>, alphas: &[u8], depth: u8) -> Vec<u32> {
    if palette.is_empty() {
        return palette;
    }
    let mut entries = palette;
    for (index, entry) in entries.iter_mut().enumerate() {
        if let Some(alpha) = alphas.get(index) {
            *entry = (*entry & 0x00ff_ffff) | (u32::from(*alpha) << 24);
        }
    }
    // A palette image reports a full table for its depth, as GDI+ does: a 4-bit
    // one has sixteen entries even if the file named fewer.
    let full = 1_usize << depth.min(8);
    entries.resize(full.max(entries.len()).min(256), 0xff00_0000);
    entries
}

/// One scanline's filter, undone against the rows it reads.
fn unfilter(filter: u8, line: &mut [u8], prior: Option<&[u8]>, bpp: usize) -> Result<(), String> {
    match filter {
        0 => Ok(()),
        1 => {
            for index in bpp..line.len() {
                line[index] = line[index].wrapping_add(line[index - bpp]);
            }
            Ok(())
        }
        2 => {
            if let Some(prior) = prior {
                for index in 0..line.len() {
                    line[index] = line[index].wrapping_add(prior[index]);
                }
            }
            Ok(())
        }
        3 => {
            for index in 0..line.len() {
                let left = if index >= bpp { u32::from(line[index - bpp]) } else { 0 };
                let above = prior.map_or(0, |prior| u32::from(prior[index]));
                line[index] = line[index].wrapping_add(((left + above) / 2) as u8);
            }
            Ok(())
        }
        4 => {
            for index in 0..line.len() {
                let left = if index >= bpp { i32::from(line[index - bpp]) } else { 0 };
                let above = prior.map_or(0, |prior| i32::from(prior[index]));
                let upper_left = if index >= bpp {
                    prior.map_or(0, |prior| i32::from(prior[index - bpp]))
                } else {
                    0
                };
                line[index] = line[index].wrapping_add(paeth(left, above, upper_left) as u8);
            }
            Ok(())
        }
        other => Err(format!("PNG row filter {other} is not one PNG defines")),
    }
}

/// The predictor filter 4 adds: whichever neighbour the gradient points at.
fn paeth(left: i32, above: i32, upper_left: i32) -> i32 {
    let estimate = left + above - upper_left;
    let (to_left, to_above) = ((estimate - left).abs(), (estimate - above).abs());
    let to_corner = (estimate - upper_left).abs();
    if to_left <= to_above && to_left <= to_corner {
        left
    } else if to_above <= to_corner {
        above
    } else {
        upper_left
    }
}

/// A scanline's samples, as RGBA.
fn expand(
    rows: &[u8],
    width: u32,
    height: u32,
    depth: u8,
    colour: u8,
    palette: &[u32],
    alphas: &[u8],
) -> Result<Vec<u8>, String> {
    let mut pixels = Vec::with_capacity(width as usize * height as usize * 4);
    let channels: u32 = match colour {
        0 | 3 => 1,
        2 => 3,
        4 => 2,
        _ => 4,
    };
    let sample_bits = u32::from(depth);
    let row_bytes = (((width as u64 * u64::from(channels) * u64::from(depth)) + 7) / 8) as usize;
    for row in 0..height as usize {
        let line = &rows[row * row_bytes..(row + 1) * row_bytes];
        let sample = |index: usize| -> u16 {
            if depth == 8 {
                u16::from(line[index])
            } else if depth == 16 {
                // Sixteen bits of a channel are more than this host's pixels
                // hold, so the high byte is the value.
                u16::from(line[index * 2])
            } else {
                let bit = index * sample_bits as usize;
                let byte = line[bit / 8];
                let shift = 8 - sample_bits as usize - bit % 8;
                u16::from((byte >> shift) & ((1u16 << sample_bits) - 1) as u8)
            }
        };
        for column in 0..width as usize {
            let at = column * channels as usize;
            let pixel = if colour == 3 {
                let index = sample(at) as usize;
                let entry = palette
                    .get(index)
                    .copied()
                    .ok_or_else(|| format!("PNG pixel names palette entry {index}, which is not there"))?;
                let alpha = alphas.get(index).copied().unwrap_or(0xff);
                Rgba::new(
                    (entry >> 16) as u8,
                    (entry >> 8) as u8,
                    entry as u8,
                    alpha,
                )
            } else {
                let scale = |value: u16| -> u8 {
                    if depth == 16 {
                        value as u8
                    } else if depth == 8 {
                        value as u8
                    } else {
                        // A packed grey sample is stretched over the byte.
                        let max = (1u16 << sample_bits) - 1;
                        (u32::from(value) * 255 / u32::from(max)) as u8
                    }
                };
                match colour {
                    0 => {
                        let grey = scale(sample(at));
                        Rgba::new(grey, grey, grey, 0xff)
                    }
                    4 => {
                        let grey = scale(sample(at));
                        Rgba::new(grey, grey, grey, scale(sample(at + 1)))
                    }
                    2 => Rgba::new(
                        scale(sample(at)),
                        scale(sample(at + 1)),
                        scale(sample(at + 2)),
                        0xff,
                    ),
                    _ => Rgba::new(
                        scale(sample(at)),
                        scale(sample(at + 1)),
                        scale(sample(at + 2)),
                        scale(sample(at + 3)),
                    ),
                }
            };
            pixels.extend_from_slice(&[pixel.red, pixel.green, pixel.blue, pixel.alpha]);
        }
    }
    Ok(pixels)
}

struct Rgba {
    red: u8,
    green: u8,
    blue: u8,
    alpha: u8,
}

impl Rgba {
    fn new(red: u8, green: u8, blue: u8, alpha: u8) -> Self {
        Self {
            red,
            green,
            blue,
            alpha,
        }
    }
}

// ---- zlib and inflate ----------------------------------------------------

/// A zlib stream, decompressed. The Adler-32 at the end is not checked: a
/// corrupt image is caught by the row filters and the sample bounds instead, and
/// the checksum would only turn a wrong picture into a failed load.
fn zlib(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    if data.len() < 2 {
        return Err("zlib stream is empty".to_string());
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 {
        return Err(format!("zlib compression method is {cmf}"));
    }
    if (u32::from(cmf) * 256 + u32::from(flg)) % 31 != 0 {
        return Err("zlib header checksum does not match".to_string());
    }
    if flg & 0x20 != 0 {
        return Err("zlib stream has a dictionary".to_string());
    }
    let mut inflater = Inflater {
        data: &data[2..],
        position: 0,
        bits: 0,
        count: 0,
    };
    let mut out: Vec<u8> = Vec::new();
    loop {
        let last = inflater.bit()?;
        let kind = inflater.take(2)?;
        match kind {
            0 => {
                inflater.align();
                let length = inflater.take(16)? as usize;
                let complement = inflater.take(16)? as usize;
                if length != !complement & 0xffff {
                    return Err("zlib stored block length does not match its complement".to_string());
                }
                if out.len() + length > limit {
                    return Err("zlib stream is longer than the image it describes".to_string());
                }
                for _ in 0..length {
                    out.push(inflater.byte()?);
                }
            }
            1 => {
                let (literals, distances) = fixed();
                block(&mut inflater, &literals, &distances, &mut out, limit)?;
            }
            2 => {
                let (literals, distances) = dynamic(&mut inflater)?;
                block(&mut inflater, &literals, &distances, &mut out, limit)?;
            }
            other => return Err(format!("deflate block type {other} is reserved")),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// One compressed block's symbols, run until the end-of-block marker.
fn block(
    inflater: &mut Inflater<'_>,
    literals: &Huffman,
    distances: &Huffman,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), String> {
    loop {
        let symbol = literals.decode(inflater)?;
        match symbol {
            0..=255 => {
                if out.len() >= limit {
                    return Err("zlib stream is longer than the image it describes".to_string());
                }
                out.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let index = (symbol - 257) as usize;
                let length = LENGTH_BASE[index] as usize
                    + inflater.take(u32::from(LENGTH_EXTRA[index]))? as usize;
                let code = distances.decode(inflater)?;
                if code > 29 {
                    return Err(format!("deflate distance code {code} is not defined"));
                }
                let distance = DISTANCE_BASE[code as usize] as usize
                    + inflater.take(u32::from(DISTANCE_EXTRA[code as usize]))? as usize;
                if distance == 0 || distance > out.len() {
                    return Err("deflate back reference points before the output".to_string());
                }
                if out.len() + length > limit {
                    return Err("zlib stream is longer than the image it describes".to_string());
                }
                let start = out.len() - distance;
                for offset in 0..length {
                    let byte = out[start + offset];
                    out.push(byte);
                }
            }
            other => return Err(format!("deflate symbol {other} is not one a block uses")),
        }
    }
}

/// The fixed Huffman code RFC 1951 defines for block type 1.
fn fixed() -> (Huffman, Huffman) {
    let mut literals = [0_u8; 288];
    for (symbol, length) in literals.iter_mut().enumerate() {
        *length = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    let distances = [5_u8; 32];
    (
        Huffman::new(&literals),
        Huffman::new(&distances),
    )
}

/// The code a block type 2 describes for itself.
fn dynamic(inflater: &mut Inflater<'_>) -> Result<(Huffman, Huffman), String> {
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let literal_count = inflater.take(5)? as usize + 257;
    let distance_count = inflater.take(5)? as usize + 1;
    let code_count = inflater.take(4)? as usize + 4;
    let mut lengths = [0_u8; 19];
    for &slot in ORDER.iter().take(code_count) {
        lengths[slot] = inflater.take(3)? as u8;
    }
    let codes = Huffman::new(&lengths);
    let mut lengths = vec![0_u8; literal_count + distance_count];
    let mut index = 0;
    while index < lengths.len() {
        let symbol = codes.decode(inflater)?;
        match symbol {
            0..=15 => {
                lengths[index] = symbol as u8;
                index += 1;
            }
            16 => {
                if index == 0 {
                    return Err("deflate repeats a code length with nothing before it".to_string());
                }
                let repeat = 3 + inflater.take(2)? as usize;
                let previous = lengths[index - 1];
                for _ in 0..repeat {
                    if index >= lengths.len() {
                        return Err("deflate code lengths run past the table".to_string());
                    }
                    lengths[index] = previous;
                    index += 1;
                }
            }
            17 | 18 => {
                let repeat = if symbol == 17 {
                    3 + inflater.take(3)? as usize
                } else {
                    11 + inflater.take(7)? as usize
                };
                for _ in 0..repeat {
                    if index >= lengths.len() {
                        return Err("deflate code lengths run past the table".to_string());
                    }
                    lengths[index] = 0;
                    index += 1;
                }
            }
            other => return Err(format!("deflate code length symbol {other} is not defined")),
        }
    }
    Ok((
        Huffman::new(&lengths[..literal_count]),
        Huffman::new(&lengths[literal_count..]),
    ))
}

/// A canonical Huffman code: how many symbols there are of each length, and the
/// symbols in code order.
struct Huffman {
    counts: [u16; 16],
    symbols: Vec<u16>,
}

impl Huffman {
    fn new(lengths: &[u8]) -> Self {
        let mut counts = [0_u16; 16];
        for &length in lengths {
            counts[usize::from(length)] += 1;
        }
        // Length zero is "no symbol", not a code of no bits.
        counts[0] = 0;
        let mut offsets = [0_u16; 16];
        for length in 1..15 {
            offsets[length + 1] = offsets[length] + counts[length];
        }
        let mut symbols = vec![0_u16; lengths.len()];
        for (symbol, &length) in lengths.iter().enumerate() {
            if length != 0 {
                symbols[offsets[usize::from(length)] as usize] = symbol as u16;
                offsets[usize::from(length)] += 1;
            }
        }
        Self { counts, symbols }
    }

    fn decode(&self, inflater: &mut Inflater<'_>) -> Result<u16, String> {
        let mut code = 0_i32;
        let mut first = 0_i32;
        let mut index = 0_i32;
        for length in 1..16 {
            code |= inflater.bit()? as i32;
            let count = i32::from(self.counts[length]);
            if code - count < first {
                let symbol = index + (code - first);
                return self
                    .symbols
                    .get(symbol as usize)
                    .copied()
                    .ok_or_else(|| "deflate code names a symbol past its table".to_string());
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err("deflate code is longer than fifteen bits".to_string())
    }
}

/// Bit reader over a deflate stream: least significant bit first.
struct Inflater<'a> {
    data: &'a [u8],
    position: usize,
    bits: u32,
    count: u32,
}

impl Inflater<'_> {
    fn bit(&mut self) -> Result<u32, String> {
        if self.count == 0 {
            let byte = *self
                .data
                .get(self.position)
                .ok_or_else(|| "zlib stream ended in the middle of a symbol".to_string())?;
            self.position += 1;
            self.bits = u32::from(byte);
            self.count = 8;
        }
        let value = self.bits & 1;
        self.bits >>= 1;
        self.count -= 1;
        Ok(value)
    }

    fn take(&mut self, count: u32) -> Result<u32, String> {
        let mut value = 0;
        for index in 0..count {
            value |= self.bit()? << index;
        }
        Ok(value)
    }

    /// Drops the bits left in the current byte, which a stored block starts on a
    /// byte boundary.
    fn align(&mut self) {
        self.bits = 0;
        self.count = 0;
    }

    fn byte(&mut self) -> Result<u8, String> {
        let byte = *self
            .data
            .get(self.position)
            .ok_or_else(|| "zlib stream ended in the middle of a stored block".to_string())?;
        self.position += 1;
        Ok(byte)
    }
}

const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// A PNG chunk: length, type, data, and a checksum the decoder does not read.
    fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&0_u32.to_be_bytes());
        out
    }

    /// A zlib stream holding `data` uncompressed: a stored deflate block, which is
    /// the one encoding a test can write down without a compressor.
    fn zlib_stored(data: &[u8]) -> Vec<u8> {
        assert!(data.len() < 0xffff);
        let mut out = vec![0x78, 0x01, 0x01];
        out.extend_from_slice(&(data.len() as u16).to_le_bytes());
        out.extend_from_slice(&(!(data.len() as u16)).to_le_bytes());
        out.extend_from_slice(data);
        // The Adler-32 the stream ends with, which this decoder skips.
        out.extend_from_slice(&[0, 0, 0, 0]);
        out
    }

    /// A PNG of `width` by `height` whose scanlines are `scanlines`: each row is
    /// its filter byte and then its packed samples, exactly as the format stores
    /// them.
    fn png(
        width: u32,
        height: u32,
        depth: u8,
        colour: u8,
        extra: &[(&[u8], Vec<u8>)],
        scanlines: &[u8],
    ) -> Vec<u8> {
        let mut out = SIGNATURE.to_vec();
        let mut header = Vec::new();
        header.extend_from_slice(&width.to_be_bytes());
        header.extend_from_slice(&height.to_be_bytes());
        header.extend_from_slice(&[depth, colour, 0, 0, 0]);
        out.extend(chunk(b"IHDR", &header));
        for (kind, data) in extra {
            out.extend(chunk(kind, data));
        }
        out.extend(chunk(b"IDAT", &zlib_stored(scanlines)));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    #[test]
    fn a_rgb_png_comes_out_as_rgba() {
        // Two pixels of red, then two of green, one row each.
        let image = png(
            2,
            2,
            8,
            2,
            &[],
            &[0, 255, 0, 0, 255, 0, 0, 0, 0, 255, 0, 0, 255, 0],
        );
        let decoded = decode_png(&image).expect("the image decodes");
        assert_eq!((decoded.width, decoded.height), (2, 2));
        assert_eq!(decoded.colour_type, 2);
        assert!(decoded.palette.is_empty());
        assert_eq!(
            decoded.pixels,
            vec![255, 0, 0, 255, 255, 0, 0, 255, 0, 255, 0, 255, 0, 255, 0, 255]
        );
    }

    #[test]
    fn a_two_bit_palette_png_packs_four_pixels_a_byte() {
        // The palette's second entry is half transparent, which `tRNS` says.
        let image = png(
            4,
            1,
            2,
            3,
            &[
                (b"PLTE", vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 255, 255, 255]),
                (b"tRNS", vec![255, 128]),
            ],
            &[0, 0b00_01_10_11],
        );
        let decoded = decode_png(&image).expect("the image decodes");
        assert_eq!(decoded.colour_type, 3);
        assert_eq!(decoded.bit_depth, 2);
        // A palette image reports a table for its depth, every entry of it.
        assert_eq!(decoded.palette.len(), 4);
        assert_eq!(decoded.palette[1], 0x80ff_0000);
        assert_eq!(
            decoded.pixels,
            vec![
                0, 0, 0, 255, // entry 0
                255, 0, 0, 128, // entry 1
                0, 255, 0, 255, // entry 2
                255, 255, 255, 255, // entry 3
            ]
        );
    }

    #[test]
    fn a_row_filter_is_undone_against_the_row_above_it() {
        // Two rows of one grey, the second stored as "what the row above has".
        let image = png(1, 2, 8, 0, &[], &[0, 0x40, 2, 0x00]);
        assert_eq!(
            decode_png(&image).expect("the image decodes").pixels,
            vec![0x40, 0x40, 0x40, 255, 0x40, 0x40, 0x40, 255]
        );
    }

    #[test]
    fn something_that_is_not_a_png_is_named_as_such() {
        let Err(error) = decode_png(b"GIF89a") else {
            panic!("a GIF is not a PNG")
        };
        assert!(error.contains("not a PNG"), "{error}");

        // A PNG whose image data stops early is a failure, not a short picture.
        let mut image = png(2, 2, 8, 0, &[], &[0, 0x10, 0x10, 0, 0x10, 0x10]);
        let cut = image.len() - 40;
        image.truncate(cut);
        assert!(decode_png(&image).is_err());
    }

    #[test]
    fn an_interlaced_png_is_refused_rather_than_misread() {
        let mut image = png(1, 1, 8, 0, &[], &[0, 0x20]);
        // The interlace byte of the header is the last of its thirteen.
        let at = 8 + 8 + 12;
        image[at] = 1;
        let Err(error) = decode_png(&image) else {
            panic!("interlacing is not decoded")
        };
        assert!(error.contains("interlaced"), "{error}");
    }
}
