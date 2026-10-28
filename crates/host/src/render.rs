//! The command stream a Direct3D 8 device writes and a page's GPU backend drains.
//!
//! Direct3D is stateful: the guest sets a transform, a render state and a stream
//! source, then draws, and the draw means whatever those settings said at that
//! moment. The browser's rendering API is on the other side of the wasm boundary,
//! so the device records each change here and the page replays the records in
//! order against its own mirror of the same state. Order is the contract —
//! replaying out of order draws with the wrong matrix or the wrong texture, which
//! looks like a geometry bug rather than a stream bug.
//!
//! # Shape
//!
//! Every record is a little-endian `u32` opcode, a little-endian `u32` payload
//! length in bytes, then that many payload bytes. A reader that does not know an
//! opcode skips it by length, which is what lets the page and the host be built at
//! different times. There is no alignment requirement: payloads are written
//! field by field, so a record of three `u32`s is twelve bytes with a twelve-byte
//! length.
//!
//! # Why a `Vec` and not guest memory
//!
//! The arena is guest-visible; a page must not be able to point a guest write at
//! the command stream. So the stream lives in the host and is exported as a
//! pointer and a length (see `wh_render_ptr` in `crates/host-wasm`), the same way
//! the audio event queue is. Vertex and texture bytes are *not* copied through
//! here: their records carry a guest offset, and the page reads the pixels or
//! vertices out of linear memory itself.
//!
//! # Addresses
//!
//! A guest address in a payload is written as an offset from
//! [`crate::pe::IMAGE_BASE`], matching `wh_framebuffer_offset`, so the page adds
//! `wh_memory_ptr()` and never needs to know the image base.
//!
//! # Running out of room
//!
//! [`Stream::begin`] has no fallible path: the stream grows as needed. A page
//! drains it every frame, so in practice it holds one frame of commands, and a
//! frame that is larger than memory is a bug worth a panic rather than a silently
//! dropped draw.

use crate::pe::IMAGE_BASE;

/// Record opcodes. The numbers are the ABI with the page's backend: changing one
/// changes both sides.
pub mod op {
    /// `u32 width, u32 height, u32 bits` — the back buffer the guest asked for.
    pub const CREATE_DEVICE: u32 = 1;
    /// `u32 flags, u32 colour, f32 depth, u32 stencil`.
    pub const CLEAR: u32 = 2;
    /// `u32 state, u32 value`.
    pub const SET_RENDER_STATE: u32 = 3;
    /// `u32 state, 16 × f32` — `D3DTS_*` slot and its matrix, row-major.
    pub const SET_TRANSFORM: u32 = 4;
    /// `u32 fvf`.
    pub const SET_FVF: u32 = 5;
    /// `u32 handle, u32 size, u32 fvf` — a vertex buffer the page can allocate.
    pub const CREATE_VERTEX_BUFFER: u32 = 6;
    /// `u32 handle, u32 offset, u32 length, u32 guest offset`.
    pub const UPDATE_VERTEX_BUFFER: u32 = 7;
    /// `u32 stream, u32 handle, u32 stride, u32 base offset`.
    pub const SET_STREAM_SOURCE: u32 = 8;
    /// `u32 handle, u32 size` — an index buffer, 16-bit indices.
    pub const CREATE_INDEX_BUFFER: u32 = 9;
    /// `u32 handle, u32 offset, u32 length, u32 guest offset`.
    pub const UPDATE_INDEX_BUFFER: u32 = 10;
    /// `u32 handle, u32 base offset` — 0 unbinds.
    pub const SET_INDICES: u32 = 11;
    /// `u32 topology, u32 first vertex, u32 vertex count`.
    pub const DRAW: u32 = 12;
    /// `u32 topology, u32 min index, u32 vertex count, u32 first index, u32
    /// primitive count`.
    pub const DRAW_INDEXED: u32 = 13;
    /// `u32 topology, u32 stride, u32 fvf, u32 guest offset, u32 vertex count`.
    pub const DRAW_UP: u32 = 14;
    /// `u32 topology, u32 stride, u32 fvf, u32 index guest offset, u32 vertex
    /// guest offset, u32 min index, u32 vertex count, u32 primitive count`.
    pub const DRAW_INDEXED_UP: u32 = 15;
    /// `u32 handle, u32 width, u32 height, u32 levels, u32 format, u32 size` —
    /// `size` is level 0's pixel bytes, which is all that is allocated.
    pub const CREATE_TEXTURE: u32 = 16;
    /// `u32 handle, u32 level, u32 guest offset, u32 pitch, u32 width, u32
    /// height, u32 format`.
    pub const UPDATE_TEXTURE: u32 = 17;
    /// `u32 stage, u32 handle` — 0 unbinds.
    pub const SET_TEXTURE: u32 = 18;
    /// `u32 stage, u32 kind, u32 value` — a `D3DTSS_*` state.
    pub const SET_TEXTURE_STAGE_STATE: u32 = 19;
    /// `u32 x, u32 y, u32 width, u32 height, f32 min z, f32 max z`.
    pub const SET_VIEWPORT: u32 = 20;
    /// `u32 state, u32 value` — the answer to `GetRenderState`, not a change.
    pub const SET_MATERIAL: u32 = 21;
    /// No payload: the guest presented a frame.
    pub const PRESENT: u32 = 22;
    /// `u32 handle` — the guest destroyed a resource, so the page may free it.
    pub const DESTROY: u32 = 23;
}

/// Guest address as the page wants it: an offset from the start of the image.
pub fn offset(address: u32) -> u32 {
    address.wrapping_sub(IMAGE_BASE)
}

/// The bytes the page drains, and the building of them.
#[derive(Default)]
pub struct Stream {
    bytes: Vec<u8>,
}

impl Stream {
    /// Starts a record. The length is filled in when the [`Record`] is dropped,
    /// so a caller cannot write a payload the header does not describe.
    pub fn begin(&mut self, op: u32) -> Record<'_> {
        let length_at = self.bytes.len() + 4;
        self.bytes.extend_from_slice(&op.to_le_bytes());
        self.bytes.extend_from_slice(&0_u32.to_le_bytes());
        Record {
            stream: self,
            length_at,
        }
    }

    /// Everything written since the last [`Stream::clear`].
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Empties the stream: the page has drained it, or is starting over.
    pub fn clear(&mut self) {
        self.bytes.clear();
    }
}

/// One record being written. Dropping it closes the record.
pub struct Record<'a> {
    stream: &'a mut Stream,
    length_at: usize,
}

impl Record<'_> {
    pub fn u32(&mut self, value: u32) -> &mut Self {
        self.stream.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    pub fn i32(&mut self, value: i32) -> &mut Self {
        self.u32(value as u32)
    }

    pub fn f32(&mut self, value: f32) -> &mut Self {
        self.stream.bytes.extend_from_slice(&value.to_le_bytes());
        self
    }

    /// A guest address, written as an offset from the image base.
    pub fn guest(&mut self, address: u32) -> &mut Self {
        self.u32(offset(address))
    }

    /// Sixteen floats, row-major, which is how `D3DMATRIX` is laid out.
    pub fn matrix(&mut self, values: &[f32; 16]) -> &mut Self {
        for value in values {
            self.f32(*value);
        }
        self
    }
}

impl Drop for Record<'_> {
    fn drop(&mut self) {
        let length = (self.stream.bytes.len() - self.length_at - 4) as u32;
        self.stream.bytes[self.length_at..self.length_at + 4]
            .copy_from_slice(&length.to_le_bytes());
    }
}

/// A reader over a stream, which is what a test and the page's backend both are.
///
/// It yields whole records and refuses a truncated one by name rather than
/// reading past the end: a stream cut short is a host bug, and the page would
/// otherwise draw a partial frame and look like a renderer bug.
pub struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    pub fn next(&mut self) -> Option<std::result::Result<(u32, &'a [u8]), String>> {
        if self.at >= self.bytes.len() {
            return None;
        }
        if self.at + 8 > self.bytes.len() {
            return Some(Err("a record header is cut short".to_string()));
        }
        let op = u32::from_le_bytes(self.bytes[self.at..self.at + 4].try_into().unwrap());
        let length =
            u32::from_le_bytes(self.bytes[self.at + 4..self.at + 8].try_into().unwrap()) as usize;
        let start = self.at + 8;
        let end = start + length;
        if end > self.bytes.len() {
            return Some(Err(format!(
                "record {op} says {length} bytes but the stream ends first"
            )));
        }
        self.at = end;
        Some(Ok((op, &self.bytes[start..end])))
    }
}

/// `u32` at `offset` of a record payload, for the tests and for diagnostics.
pub fn payload_u32(payload: &[u8], index: usize) -> Option<u32> {
    let at = index * 4;
    let bytes = payload.get(at..at + 4)?;
    Some(u32::from_le_bytes(bytes.try_into().ok()?))
}

/// `f32` at `offset` of a record payload.
pub fn payload_f32(payload: &[u8], index: usize) -> Option<f32> {
    payload_u32(payload, index).map(f32::from_bits)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_carries_its_own_length() {
        let mut stream = Stream::default();
        stream.begin(op::CLEAR).u32(7).f32(1.0).u32(0);
        let bytes = stream.bytes();
        assert_eq!(bytes.len(), 8 + 12);
        assert_eq!(payload_u32(bytes, 0), Some(op::CLEAR));
        assert_eq!(payload_u32(bytes, 1), Some(12), "length");
    }

    #[test]
    fn a_cursor_reads_records_in_order() {
        let mut stream = Stream::default();
        stream.begin(op::SET_RENDER_STATE).u32(26).u32(1);
        stream.begin(op::DRAW).u32(4).u32(0).u32(3);
        let mut cursor = Cursor::new(stream.bytes());
        let (first, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(first, op::SET_RENDER_STATE);
        assert_eq!(payload_u32(payload, 1), Some(1));
        let (second, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(second, op::DRAW);
        assert_eq!(payload_u32(payload, 2), Some(3));
        assert!(cursor.next().is_none());
    }

    #[test]
    fn a_truncated_record_is_refused() {
        let mut stream = Stream::default();
        stream.begin(op::DRAW).u32(4).u32(0).u32(3);
        let bytes = &stream.bytes()[..12];
        let mut cursor = Cursor::new(bytes);
        assert!(cursor.next().unwrap().is_err());
    }

    #[test]
    fn guest_addresses_are_offsets_from_the_image() {
        let address = IMAGE_BASE + 0x1234;
        assert_eq!(offset(address), 0x1234);
        let mut stream = Stream::default();
        stream.begin(op::SET_STREAM_SOURCE).u32(0).u32(7).u32(32).guest(address);
        let mut cursor = Cursor::new(stream.bytes());
        let (_, payload) = cursor.next().unwrap().unwrap();
        assert_eq!(payload_u32(payload, 3), Some(0x1234));
    }

    #[test]
    fn clear_empties_the_stream() {
        let mut stream = Stream::default();
        stream.begin(op::PRESENT);
        assert!(!stream.is_empty());
        stream.clear();
        assert!(stream.is_empty());
    }
}
