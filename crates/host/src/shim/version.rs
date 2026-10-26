//! `VERSION.dll`: the version resource an image carries, served to a guest that
//! reads it.
//!
//! The three calls are a small protocol. `GetFileVersionInfoSizeA` asks how large
//! the block is, `GetFileVersionInfoA` fills a buffer with it, and
//! `VerQueryValueA` names a piece inside that buffer. The block itself is the
//! image's own `RT_VERSION` resource, already mapped, so this module copies it
//! once and then answers offsets into the copy.
//!
//! `VerQueryValueA` is the only part with a shape to it:
//!
//! - `"\\"` names the `VS_FIXEDFILEINFO`, a 52-byte structure that starts with
//!   the signature `0xFEEF04BD`.
//! - `"\\StringFileInfo\\<lang>\\<key>"` and `"\\VarFileInfo\\<key>"` name a node
//!   of the tree the block is. Every node is `wLength, wValueLength, wType,
//!   szKey (UTF-16, NUL-terminated), padding to a dword, value`, so a node is
//!   found by searching for its key and stepping back over the three words in
//!   front of it.
//!
//! A key the block does not have is a clean failure -- 0 and no value -- which is
//! what Windows answers.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const VERSION: &str = "VERSION.dll";

/// `RT_VERSION`, the resource a block is copied out of.
const RT_VERSION: u32 = 16;

/// `VS_FFI_SIGNATURE`: the first dword of `VS_FIXEDFILEINFO`.
const FIXED_FILE_INFO_SIGNATURE: u32 = 0xfeef_04bd;

/// The structure's size, which is what `VerQueryValueA` reports for `"\\"`.
const FIXED_FILE_INFO_BYTES: u32 = 52;

pub const SHIMS: &[Binding] = &[
    Binding { dll: VERSION, name: "GetFileVersionInfoSizeA", arg_bytes: 8, shim: size_of_version_info },
    Binding { dll: VERSION, name: "GetFileVersionInfoA", arg_bytes: 16, shim: get_version_info },
    Binding { dll: VERSION, name: "VerQueryValueA", arg_bytes: 16, shim: query_value },
];

/// `GetFileVersionInfoSizeA(file, &handle)`.
///
/// The image in this machine is the only file that can carry a version resource,
/// and its resource tree is where any answer comes from. A name that is not the
/// guest's own executable has no block, which is a 0 and an untouched handle, as
/// on Windows.
fn size_of_version_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(size) = version_resource(host).map(|(_, size)| size) else {
        return Ok(Return(0));
    };
    let out = call.arg(1)?;
    if out != 0 {
        call.write_u32(out, 0)?;
    }
    Ok(Return(size))
}

/// `GetFileVersionInfoA(file, handle, length, block)`: the resource copied into
/// the guest's buffer.
fn get_version_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some((source, size)) = version_resource(host) else {
        return Ok(Return(0));
    };
    let requested = call.arg(2)?;
    let block = call.arg(3)?;
    if block == 0 || requested < size {
        return Ok(Return(0));
    }
    let bytes = call.memory.read_bytes(source, size as usize)?.to_vec();
    call.write(block, &bytes)?;
    // The queries that follow are pointed at *this* copy, not at the resource, so
    // the block and its length are what the rest of the protocol works on.
    host.version = Some((block, size));
    Ok(Return(1))
}

/// `VerQueryValueA(block, sub, &value, &length)`.
fn query_value(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let sub = call.arg(1)?;
    let value_out = call.arg(2)?;
    let length_out = call.arg(3)?;
    let Some((block, size)) = host.version else {
        return Ok(Return(0));
    };
    let path = if sub == 0 {
        String::new()
    } else {
        call.memory.read_cstr(sub)?
    };

    let found = if path == "\\" || path.is_empty() {
        // The root query: the fixed file info, wherever its signature is.
        find_signature(call, block, size).map(|at| (at, FIXED_FILE_INFO_BYTES, false))
    } else {
        let key = path.rsplit('\\').next().unwrap_or(&path);
        find_node(call, block, size, key)
    };
    let Some((address, length, is_text)) = found else {
        return Ok(Return(0));
    };
    if value_out != 0 {
        call.write_u32(value_out, address)?;
    }
    if length_out != 0 {
        // A string node counts characters, which for UTF-16 is its byte length
        // halved; a binary node counts bytes.
        let reported = if is_text { length / 2 } else { length };
        call.write_u32(length_out, reported)?;
    }
    Ok(Return(1))
}

/// The image's `RT_VERSION` resource: its guest address and its length.
fn version_resource(host: &Host) -> Option<(u32, u32)> {
    let resource = host
        .resources
        .iter()
        .find(|resource| resource.kind == crate::pe::ResourceKey::Id(RT_VERSION))?;
    Some((crate::pe::IMAGE_BASE.wrapping_add(resource.rva), resource.size))
}

/// Where `VS_FIXEDFILEINFO` starts inside the block: the first dword matching its
/// signature, which is the standard way to find it without parsing.
fn find_signature(call: &mut Call<'_>, block: u32, size: u32) -> Option<u32> {
    let bytes = call.memory.read_bytes(block, size as usize).ok()?;
    let signature = FIXED_FILE_INFO_SIGNATURE.to_le_bytes();
    let at = bytes
        .windows(4)
        .position(|window| window == signature)?;
    Some(block + at as u32)
}

/// A node of the version tree, found by its key.
///
/// `is_text` is the node's `wType`: 1 means its value is a UTF-16 string, which
/// `VerQueryValueA` measures in characters rather than bytes.
fn find_node(call: &mut Call<'_>, block: u32, size: u32, key: &str) -> Option<(u32, u32, bool)> {
    let bytes = call.memory.read_bytes(block, size as usize).ok()?;
    let mut wanted = Vec::with_capacity(key.len() * 2 + 2);
    for unit in key.encode_utf16() {
        wanted.extend_from_slice(&unit.to_le_bytes());
    }
    wanted.extend_from_slice(&[0, 0]);

    let mut at = 0_usize;
    while at + wanted.len() <= bytes.len() {
        let found = bytes[at..]
            .windows(wanted.len())
            .position(|window| window == wanted.as_slice())?;
        let key_at = at + found;
        if key_at < 6 {
            return None;
        }
        // The node's three leading words start six bytes before its key.
        let node = key_at - 6;
        let value_length = u16::from_le_bytes([bytes[node + 2], bytes[node + 3]]) as u32;
        let kind = u16::from_le_bytes([bytes[node + 4], bytes[node + 5]]);
        let after_key = key_at + wanted.len();
        let value = (after_key + 3) & !3;
        if value + value_length as usize <= bytes.len() {
            return Some((block + value as u32, value_length, kind == 1));
        }
        // A key whose value runs past the block is the wrong key: a shorter match
        // inside another node's name, for instance.
        at = key_at + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    #[test]
    fn a_key_is_found_inside_a_version_block() {
        // `VS_VERSIONINFO` holding one string node: key "ProductVersion",
        // value "1.2.3".
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let block = STACK_BASE + 0x8000;
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // a header of three words
        for unit in "ProductVersion".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes.extend_from_slice(&[0, 0]); // the key's terminator
        while bytes.len() % 4 != 0 {
            bytes.push(0);
        }
        for unit in "1.2.3".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        // `wValueLength` counts the characters of the value.
        bytes[2..4].copy_from_slice(&5_u16.to_le_bytes());
        bytes[4..6].copy_from_slice(&1_u16.to_le_bytes());
        memory.write_bytes(block, &bytes).unwrap();
        host.version = Some((block, bytes.len() as u32));

        let out = STACK_BASE + 0x9000;
        let length = STACK_BASE + 0x9100;
        let arguments = [block, STACK_BASE + 0x9200, out, length];
        let args = STACK_BASE + 0x4000;
        memory
            .write_bytes(
                args,
                &arguments
                    .iter()
                    .flat_map(|value| value.to_le_bytes())
                    .collect::<Vec<u8>>(),
            )
            .unwrap();
        memory
            .write_bytes(STACK_BASE + 0x9200, b"\\StringFileInfo\\040904B0\\ProductVersion\0")
            .unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 16,
        };
        assert_eq!(query_value(&mut host, &mut call).unwrap(), Return(1));
        assert_eq!(call.memory.read_u32(length).unwrap(), 5);
        let value = call.memory.read_u32(out).unwrap();
        assert_eq!(call.memory.read_bytes(value, 10).unwrap(), b"1\02\0.\03\0.\03\0".as_slice());
    }
}
