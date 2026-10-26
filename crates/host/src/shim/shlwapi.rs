//! `shlwapi.dll`: the path helpers a program uses before it opens anything.
//!
//! A game finds its own folder with `GetModuleFileNameA` and then trims the file
//! name off with `PathRemoveFileSpecA`, which is the only reason this DLL is
//! imported at all. The path is guest text, so it is read and written as bytes.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const SHLWAPI: &str = "SHLWAPI.dll";

pub const SHIMS: &[Binding] = &[Binding {
    dll: SHLWAPI,
    name: "PathRemoveFileSpecA",
    arg_bytes: 4,
    shim: path_remove_file_spec,
}];

/// `PathRemoveFileSpecA(path)`: drops the last component of a path, and the
/// separator in front of it. Returns whether anything was removed, and leaves a
/// path that is already a directory alone -- which is how a caller tells
/// `C:\games\` from `C:\games\game.exe`.
fn path_remove_file_spec(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = call.arg(0)?;
    if path == 0 {
        return Ok(Return(0));
    }
    // The path is guest text in the ANSI codepage, so it is read as bytes: a
    // `String` would replace every byte that is not UTF-8.
    let mut bytes = Vec::new();
    for offset in 0..4096_u32 {
        let byte = call.memory.read_u8(path.wrapping_add(offset))?;
        if byte == 0 {
            break;
        }
        bytes.push(byte);
    }
    let mut end = bytes.len();
    // A trailing separator is part of the directory, not a file name.
    while end > 0 && matches!(bytes[end - 1], b'\\' | b'/') {
        end -= 1;
    }
    let Some(separator) = bytes[..end].iter().rposition(|byte| matches!(byte, b'\\' | b'/')) else {
        return Ok(Return(0));
    };
    // Keep a drive root (`C:\`) as it is: it has no file name to remove.
    if separator == 0 {
        return Ok(Return(0));
    }
    let kept = if separator == 2 && bytes[1] == b':' {
        separator + 1
    } else {
        separator
    };
    call.write(path + kept as u32, &[0])?;
    Ok(Return(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    fn call_shim(host: &mut Host, memory: &mut GuestMemory, arguments: &[u32]) -> Result<Action> {
        let binding = &SHIMS[0];
        let args = STACK_BASE + 0xb000;
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

    #[test]
    fn a_file_name_is_trimmed_and_a_directory_is_left_alone() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let path = STACK_BASE + 0xb100;
        memory.write_bytes(path, b"C:\\games\\cave\\doukutsu.exe\0").unwrap();
        assert_eq!(call_shim(&mut host, &mut memory, &[path]).unwrap(), Return(1));
        let text = memory.read_bytes(path, 15).unwrap();
        assert_eq!(&text[..14], &b"C:\\games\\cave\0"[..14]);
        assert_eq!(text[14], b'u', "the rest of the old name is still there");

        memory.write_bytes(path, b"C:\\games\\cave\0").unwrap();
        assert_eq!(call_shim(&mut host, &mut memory, &[path]).unwrap(), Return(0));
        assert_eq!(memory.read_bytes(path, 13).unwrap(), b"C:\\games\\cave");

        // A drive root has no file name to remove.
        memory.write_bytes(path, b"C:\\\0").unwrap();
        assert_eq!(call_shim(&mut host, &mut memory, &[path]).unwrap(), Return(0));
        assert_eq!(memory.read_bytes(path, 4).unwrap(), b"C:\\\0");
    }
}
