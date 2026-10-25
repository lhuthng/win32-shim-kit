//! The `ADVAPI32.dll` registry the boot needs: four calls over one in-memory
//! tree.
//!
//! A guest keeps its installed settings in `HKLM\SOFTWARE\Vendor\Product`,
//! value `Data`, as `REG_BINARY`: on the first run it asks for the value's size,
//! gets `ERROR_FILE_NOT_FOUND`, writes a four-byte default, builds its defaults
//! and writes the real blob back. That is a path we can serve truthfully, but
//! only if the key exists -- and a rehost has no installer to create it. So
//! opening a missing key succeeds when the guest asks for write access and fails
//! with `ERROR_FILE_NOT_FOUND` when it asks read-only. That is the one deviation
//! from Windows in this file, and it stands in for the installer rather than for
//! the registry.
//!
//! The tree is per session and never touches the disk. Saving across sessions is
//! the M6 save/load work: the page will put this tree in `localStorage` or OPFS
//! once there is something worth keeping. Nothing here parses the blob, so a
//! value is bytes and a type, nothing more.

use std::collections::HashMap;

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const ADVAPI32: &str = "ADVAPI32.dll";

const ERROR_SUCCESS: u32 = 0;
const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_MORE_DATA: u32 = 234;

/// `KEY_SET_VALUE | KEY_CREATE_SUB_KEY`, the two rights that imply the guest
/// intends to write. `KEY_ALL_ACCESS` (`0x000f003f`) contains both.
const KEY_WRITE: u32 = 0x0000_0002 | 0x0000_0004;

const FIRST_KEY: u32 = 0x0004_0000;

/// One stored value: its `REG_*` type and its bytes.
#[derive(Clone)]
struct Value {
    kind: u32,
    data: Vec<u8>,
}

/// The five predefined hives, by the handle Windows assigns them.
fn hive(handle: u32) -> Option<&'static str> {
    Some(match handle {
        0x8000_0000 => "HKCR",
        0x8000_0001 => "HKCU",
        0x8000_0002 => "HKLM",
        0x8000_0003 => "HKU",
        0x8000_0005 => "HKCC",
        _ => return None,
    })
}

/// The registry tree and the handles open on it.
#[derive(Default)]
pub struct Registry {
    /// Keys by full path, lowercased: the registry is case-insensitive and this
    /// host has no reason to keep the guest's spelling.
    keys: HashMap<String, HashMap<String, Value>>,
    handles: HashMap<u32, String>,
    next_handle: u32,
}

impl Registry {
    fn open(&mut self, path: String, create: bool) -> Option<u32> {
        if !self.keys.contains_key(&path) {
            if !create {
                return None;
            }
            self.keys.insert(path.clone(), HashMap::new());
        }
        let handle = FIRST_KEY + self.next_handle * 4;
        self.next_handle += 1;
        self.handles.insert(handle, path);
        Some(handle)
    }

    fn path_of(&self, handle: u32) -> Option<&String> {
        self.handles.get(&handle)
    }
}

pub const SHIMS: &[Binding] = &[
    Binding { dll: ADVAPI32, name: "RegCloseKey", arg_bytes: 4, shim: reg_close_key },
    Binding { dll: ADVAPI32, name: "RegOpenKeyExA", arg_bytes: 20, shim: reg_open_key_ex },
    Binding { dll: ADVAPI32, name: "RegQueryValueExA", arg_bytes: 24, shim: reg_query_value_ex },
    Binding { dll: ADVAPI32, name: "RegSetValueExA", arg_bytes: 24, shim: reg_set_value_ex },
];

fn reg_open_key_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parent = call.arg(0)?;
    let subkey = optional_cstr(call, 1)?;
    let access = call.arg(3)?;
    let destination = call.arg(4)?;
    if destination == 0 {
        return Ok(Return(ERROR_INVALID_HANDLE));
    }

    // A parent is either a predefined hive or a key this host handed out, which
    // is how `RegOpenKeyExA` on an open key is written.
    let base = match hive(parent) {
        Some(name) => name.to_string(),
        None => match host.registry.path_of(parent) {
            Some(path) => path.clone(),
            None => return Ok(Return(ERROR_INVALID_HANDLE)),
        },
    };
    let path = if subkey.is_empty() {
        base
    } else {
        format!("{}\\{}", base, subkey)
    };
    let path = path.to_lowercase();

    let create = access & KEY_WRITE != 0;
    match host.registry.open(path, create) {
        Some(handle) => {
            call.write_u32(destination, handle)?;
            Ok(Return(ERROR_SUCCESS))
        }
        None => Ok(Return(ERROR_FILE_NOT_FOUND)),
    }
}

fn reg_close_key(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    match host.registry.handles.remove(&handle) {
        Some(_) => Ok(Return(ERROR_SUCCESS)),
        // Closing a hive handle is legal and does nothing on Windows, so only
        // an unknown handle is an error.
        None if hive(handle).is_some() => Ok(Return(ERROR_SUCCESS)),
        None => Ok(Return(ERROR_INVALID_HANDLE)),
    }
}

fn reg_query_value_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let name = optional_cstr(call, 1)?.to_lowercase();
    let kind_out = call.arg(3)?;
    let data = call.arg(4)?;
    let size_out = call.arg(5)?;

    let Some(path) = host.registry.path_of(handle).cloned() else {
        return Ok(Return(ERROR_INVALID_HANDLE));
    };
    let Some(value) = host
        .registry
        .keys
        .get(&path)
        .and_then(|key| key.get(&name))
        .cloned()
    else {
        // The guest asked for a value the installer never wrote; that is a
        // first run, not an error in the host.
        return Ok(Return(ERROR_FILE_NOT_FOUND));
    };

    if kind_out != 0 {
        call.write_u32(kind_out, value.kind)?;
    }
    let required = value.data.len() as u32;
    let capacity = if size_out != 0 {
        call.memory.read_u32(size_out)?
    } else {
        0
    };
    if data == 0 || size_out == 0 {
        // A size query: report what a read would need and succeed, which is
        // what lets the guest size its buffer in one call.
        if size_out != 0 {
            call.write_u32(size_out, required)?;
        }
        return Ok(Return(ERROR_SUCCESS));
    }
    if capacity < required {
        call.write_u32(size_out, required)?;
        return Ok(Return(ERROR_MORE_DATA));
    }
    call.write(data, &value.data)?;
    call.write_u32(size_out, required)?;
    Ok(Return(ERROR_SUCCESS))
}

fn reg_set_value_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let name = optional_cstr(call, 1)?.to_lowercase();
    let kind = call.arg(3)?;
    let data = call.arg(4)?;
    let size = call.arg(5)?;

    let Some(path) = host.registry.path_of(handle).cloned() else {
        return Ok(Return(ERROR_INVALID_HANDLE));
    };
    let bytes = if data == 0 || size == 0 {
        Vec::new()
    } else {
        call.memory.read_bytes(data, size as usize)?.to_vec()
    };
    host.registry
        .keys
        .entry(path)
        .or_default()
        .insert(name, Value { kind, data: bytes });
    Ok(Return(ERROR_SUCCESS))
}

/// A string argument, where null means an empty name rather than address 0.
fn optional_cstr(call: &Call<'_>, index: u32) -> Result<String> {
    if call.arg(index)? == 0 {
        return Ok(String::new());
    }
    call.cstr(index)
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
        let args = STACK_BASE + 0x5000;
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

    /// Opens `HKLM\SOFTWARE\Vendor\Product` for writing, the way a first-run
    /// guest does, and returns the handle.
    fn open_game_key(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        let subkey = STACK_BASE + 0x6000;
        let destination = STACK_BASE + 0x6100;
        memory
            .write_bytes(subkey, b"SOFTWARE\\Example\\Product\0")
            .unwrap();
        let action = call_shim(
            host,
            memory,
            "RegOpenKeyExA",
            &[0x8000_0002, subkey, 0, 0x000f_003f, destination],
        )
        .unwrap();
        assert_eq!(action, Return(ERROR_SUCCESS));
        memory.read_u32(destination).unwrap()
    }

    #[test]
    fn opening_for_write_creates_the_key_the_installer_would_have() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = open_game_key(&mut host, &mut memory);
        assert!(handle >= FIRST_KEY);
        assert!(host.registry.path_of(handle).unwrap().ends_with("product"));
    }

    #[test]
    fn opening_for_read_only_reports_a_missing_key() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let subkey = STACK_BASE + 0x6200;
        memory.write_bytes(subkey, b"SOFTWARE\\Nowhere\0").unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegOpenKeyExA",
                &[0x8000_0001, subkey, 0, 0x0002_0019, STACK_BASE + 0x6300]
            )
            .unwrap(),
            Return(ERROR_FILE_NOT_FOUND)
        );
    }

    #[test]
    fn a_value_round_trips_and_reports_the_size_it_needs() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = open_game_key(&mut host, &mut memory);
        let name = STACK_BASE + 0x6400;
        let source = STACK_BASE + 0x6500;
        memory.write_bytes(name, b"Data\0").unwrap();
        memory.write_bytes(source, &[1, 2, 3, 4, 5, 6]).unwrap();

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegSetValueExA",
                &[handle, name, 0, 3, source, 6]
            )
            .unwrap(),
            Return(ERROR_SUCCESS)
        );

        // A size query succeeds and reports six bytes.
        let size = STACK_BASE + 0x6600;
        memory.write_u32(size, 0).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegQueryValueExA",
                &[handle, name, 0, 0, 0, size]
            )
            .unwrap(),
            Return(ERROR_SUCCESS)
        );
        assert_eq!(memory.read_u32(size).unwrap(), 6);

        // A buffer that is too small reports the size and ERROR_MORE_DATA.
        let destination = STACK_BASE + 0x6700;
        memory.write_u32(size, 4).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegQueryValueExA",
                &[handle, name, 0, 0, destination, size]
            )
            .unwrap(),
            Return(ERROR_MORE_DATA)
        );
        assert_eq!(memory.read_u32(size).unwrap(), 6);

        // The real read copies the bytes and the type.
        let kind = STACK_BASE + 0x6800;
        memory.write_u32(size, 6).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegQueryValueExA",
                &[handle, name, 0, kind, destination, size]
            )
            .unwrap(),
            Return(ERROR_SUCCESS)
        );
        assert_eq!(memory.read_u32(kind).unwrap(), 3);
        assert_eq!(memory.read_u32(size).unwrap(), 6);
        assert_eq!(memory.read_bytes(destination, 6).unwrap(), &[1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn a_missing_value_is_file_not_found() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = open_game_key(&mut host, &mut memory);
        let name = STACK_BASE + 0x6900;
        let size = STACK_BASE + 0x6a00;
        memory.write_bytes(name, b"Data\0").unwrap();
        memory.write_u32(size, 0).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegQueryValueExA",
                &[handle, name, 0, 0, 0, size]
            )
            .unwrap(),
            Return(ERROR_FILE_NOT_FOUND)
        );
    }

    #[test]
    fn a_closed_handle_is_no_longer_usable() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = open_game_key(&mut host, &mut memory);
        assert_eq!(
            call_shim(&mut host, &mut memory, "RegCloseKey", &[handle]).unwrap(),
            Return(ERROR_SUCCESS)
        );
        let name = STACK_BASE + 0x6b00;
        memory.write_bytes(name, b"Data\0").unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegQueryValueExA",
                &[handle, name, 0, 0, 0, 0]
            )
            .unwrap(),
            Return(ERROR_INVALID_HANDLE)
        );
    }
}
