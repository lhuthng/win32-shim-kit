//! The guest's files, as a table the host fills and a shim reads.
//!
//! The rehost never opens a path itself. The page (or the native harness) fills
//! [`Files`] with the bytes the guest is allowed to see, and `CreateFileA` looks
//! a name up in that table. That is what keeps the wasm module free of imports
//! and the guest away from the machine's disk: file I/O is a host-supplied set
//! of bytes, nothing more.
//!
//! Names are matched the way the VFS on the page matches them -- by basename,
//! case-insensitively -- because the guest builds full paths from a registry
//! value that has no meaning in a browser (`C:\Games\sfx.dat`) while the
//! bundle is flat.
//!
//! # Two kinds of file
//!
//! The bundle is the installer's output and is read-only: a guest that asks to
//! write one of those files gets `ERROR_ACCESS_DENIED` rather than a handle onto
//! bytes it cannot change.
//!
//! Anything else the guest opens for writing is a **saved file** -- the game's
//! own `save0001.dat` and friends. It lives in the same table, it is writable,
//! and it is the only thing here that outlives a session: the page copies the
//! changed files out through the `wh_saved_*` exports and keeps them in browser
//! storage, then hands them back with [`Files::provide_saved`] before the guest
//! starts. No path is opened on the machine, so a page served over the network
//! writes nothing anywhere but its own storage.

use std::collections::{BTreeSet, HashMap};

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const KERNEL32: &str = "KERNEL32.dll";

const INVALID_HANDLE_VALUE: u32 = 0xffff_ffff;
const INVALID_SET_FILE_POINTER: u32 = 0xffff_ffff;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_INVALID_HANDLE: u32 = 6;

const GENERIC_WRITE: u32 = 0x4000_0000;
const FILE_WRITE_DATA: u32 = 0x0000_0002;

const CREATE_NEW: u32 = 1;
const CREATE_ALWAYS: u32 = 2;
const OPEN_EXISTING: u32 = 3;
const OPEN_ALWAYS: u32 = 4;
const TRUNCATE_EXISTING: u32 = 5;

const FILE_BEGIN: u32 = 0;
const FILE_CURRENT: u32 = 1;
const FILE_END: u32 = 2;

const FILE_TYPE_DISK: u32 = 1;

/// First handle this table hands out; disjoint from the heap, window and
/// registry ranges so a log line is unambiguous.
const FIRST_FILE: u32 = 0x0006_0000;

/// Search handles from `FindFirstFileA`, above the file handles.
const FIRST_SEARCH: u32 = 0x0007_0000;

/// `WIN32_FIND_DATAA`.
const FIND_DATA_SIZE: u32 = 0x140;

/// A file the guest has open: which entry, and where its cursor is.
struct Open {
    key: String,
    position: u32,
}

/// One provided file.
///
/// A **synthetic** entry has a length and no bytes: every read returns zeroes.
/// It exists for the one file the guest only ever measures -- the CD's
/// `data1.cab`, which it opens and compares against 150,000,000 bytes to decide
/// that the disc is in the drive. Handing it 150 MB of real zeroes would cost the
/// page 150 MB for a number.
struct Entry {
    bytes: Vec<u8>,
    length: u32,
    /// Whether the guest may write it. The bundle is not; a save is.
    writable: bool,
}

/// Every byte the guest can read, and the handles open on it.
#[derive(Default)]
pub struct Files {
    /// Lowercased full path as provided.
    entries: HashMap<String, Entry>,
    /// Basename, lowercased, to the entry key. The guest builds its own paths.
    by_name: HashMap<String, String>,
    open: HashMap<u32, Open>,
    next_handle: u32,
    /// Handles `FindFirstFileA` handed out, so `FindClose` has something to
    /// close. The find data is written at call time, so a handle is only a token.
    searches: BTreeSet<u32>,
    /// Bumped whenever a writable file changes, which is how the page knows the
    /// guest saved something without being told what a save is.
    version: u64,
    /// Bytes this table has handed to the guest, which is how a session test can
    /// tell "the guest opened a file" from "the guest read one".
    bytes_read: u64,
    /// Reads served from a synthetic entry, which is worth exactly one notice:
    /// the guest measuring a marker is expected, the guest *reading* one is not.
    synthetic_reads: u32,
}

impl Files {
    /// Adds a file the guest may open. `path` may be a plain name or a path; the
    /// basename is what a lookup uses.
    pub fn provide(&mut self, path: &str, bytes: Vec<u8>) {
        let key = normalize(path);
        let length = bytes.len() as u32;
        let base = basename(&key);
        self.by_name.insert(base, key.clone());
        self.entries.insert(
            key,
            Entry {
                bytes,
                length,
                writable: false,
            },
        );
    }

    /// Adds a file the guest saved in an earlier session. It is writable, so the
    /// guest can overwrite it and the change is what the page persists next.
    pub fn provide_saved(&mut self, path: &str, bytes: Vec<u8>) {
        let key = normalize(path);
        let base = basename(&key);
        let length = bytes.len() as u32;
        self.by_name.insert(base, key.clone());
        self.entries.insert(
            key,
            Entry {
                bytes,
                length,
                writable: true,
            },
        );
    }

    /// Provides a file of `length` zero bytes that costs nothing to hold.
    pub fn provide_sized(&mut self, path: &str, length: u32) {
        let key = normalize(path);
        let base = basename(&key);
        self.by_name.insert(base, key.clone());
        self.entries.insert(
            key,
            Entry {
                bytes: Vec::new(),
                length,
                writable: false,
            },
        );
    }

    /// Whether the entry behind `path` is synthetic, which the session tests use
    /// to show a marker is a marker.
    pub fn synthetic(&self, path: &str) -> bool {
        self.resolve(path)
            .and_then(|key| self.entries.get(&key))
            .is_some_and(|entry| entry.bytes.is_empty() && entry.length > 0)
    }

    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.entries.values().map(|entry| entry.bytes.len()).sum()
    }

    /// Bytes the guest could read if it wanted it all.
    pub fn total_length(&self) -> u64 {
        self.entries.values().map(|entry| u64::from(entry.length)).sum()
    }

    /// Every provided name, in the order a directory listing would want them.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self.entries.keys().map(String::as_str).collect();
        names.sort_unstable();
        names
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    fn resolve(&self, path: &str) -> Option<String> {
        let key = normalize(path);
        if self.entries.contains_key(&key) {
            return Some(key);
        }
        self.by_name.get(&basename(&key)).cloned()
    }

    fn open(&mut self, path: &str) -> Option<u32> {
        let key = self.resolve(path)?;
        let handle = FIRST_FILE + self.next_handle * 4;
        self.next_handle += 1;
        self.open.insert(handle, Open { key, position: 0 });
        Some(handle)
    }

    pub fn close(&mut self, handle: u32) -> bool {
        self.open.remove(&handle).is_some()
    }

    /// The handle's file, or `None` if the guest never opened it.
    fn file(&self, handle: u32) -> Option<(&Entry, u32)> {
        let open = self.open.get(&handle)?;
        let entry = self.entries.get(&open.key)?;
        Some((entry, open.position))
    }

    fn seek(&mut self, handle: u32, position: u32) {
        if let Some(open) = self.open.get_mut(&handle) {
            open.position = position;
        }
    }

    /// Whether the table holds `path` at all, whether the guest may write it or
    /// not, which separates "denied" from "not found".
    pub fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_some()
    }

    /// Whether a handle is open, for the calls that only need a valid handle.
    pub fn is_open(&self, handle: u32) -> bool {
        self.open.contains_key(&handle)
    }

    /// Whether the entry behind `path` is one the guest wrote, so a delete or an
    /// overwrite may touch it.
    pub fn saved(&self, path: &str) -> bool {
        self.resolve(path)
            .and_then(|key| self.entries.get(&key))
            .is_some_and(|entry| entry.writable)
    }

    /// Opens `path` for writing, creating or truncating it as `creation` asks.
    /// Returns `None` when the request contradicts what the table holds -- a
    /// bundle file the guest wants to overwrite, or a save it wants to read that
    /// is not there -- so the caller can answer with the right Win32 error.
    pub fn create(&mut self, path: &str, creation: u32) -> Option<u32> {
        const CREATE_NEW: u32 = 1;
        const CREATE_ALWAYS: u32 = 2;
        const OPEN_EXISTING: u32 = 3;
        const TRUNCATE_EXISTING: u32 = 5;

        let wanted = normalize(path);
        let key = match self.resolve(path) {
            Some(existing) => {
                if !self.entries.get(&existing)?.writable {
                    // Nothing may replace a file the bundle supplied.
                    return None;
                }
                if creation == CREATE_NEW {
                    return None;
                }
                existing
            }
            None => {
                if matches!(creation, OPEN_EXISTING | TRUNCATE_EXISTING) {
                    return None;
                }
                self.by_name.insert(basename(&wanted), wanted.clone());
                self.entries.insert(
                    wanted.clone(),
                    Entry {
                        bytes: Vec::new(),
                        length: 0,
                        writable: true,
                    },
                );
                wanted
            }
        };
        if matches!(creation, CREATE_ALWAYS | TRUNCATE_EXISTING) {
            if let Some(entry) = self.entries.get_mut(&key) {
                entry.bytes.clear();
                entry.length = 0;
                self.version += 1;
            }
        }
        let handle = FIRST_FILE + self.next_handle * 4;
        self.next_handle += 1;
        self.open.insert(handle, Open { key, position: 0 });
        Some(handle)
    }

    /// Writes at the open file's cursor and advances it, growing the file as a
    /// real one grows. `None` when the handle is not open for writing.
    pub fn write(&mut self, handle: u32, bytes: &[u8]) -> Option<u32> {
        let Self { open, entries, .. } = self;
        let open = open.get_mut(&handle)?;
        let entry = entries.get_mut(&open.key)?;
        if !entry.writable {
            return None;
        }
        let start = open.position as usize;
        let end = start.checked_add(bytes.len())?;
        if entry.bytes.len() < end {
            entry.bytes.resize(end, 0);
        }
        entry.bytes[start..end].copy_from_slice(bytes);
        entry.length = entry.bytes.len() as u32;
        open.position = end as u32;
        self.version += 1;
        Some(bytes.len() as u32)
    }

    /// `SetEndOfFile`: the file becomes as long as the cursor, discarding
    /// anything past it.
    pub fn end_of_file(&mut self, handle: u32) -> bool {
        let Some(open) = self.open.get(&handle) else {
            return false;
        };
        let position = open.position as usize;
        let Some(entry) = self.entries.get_mut(&open.key) else {
            return false;
        };
        if !entry.writable {
            return false;
        }
        entry.bytes.resize(position, 0);
        entry.length = position as u32;
        self.version += 1;
        true
    }

    /// `DeleteFileA`. Only a saved file can be deleted; the bundle is not the
    /// guest's to remove.
    pub fn delete(&mut self, path: &str) -> bool {
        let key = normalize(path);
        if !self.saved(&key) {
            return false;
        }
        let Some(key) = self.resolve(&key) else {
            return false;
        };
        self.entries.remove(&key);
        self.by_name.retain(|_, entry| *entry != key);
        self.open.retain(|_, open| open.key != key);
        self.version += 1;
        true
    }

    /// The saved files, by name, which is what the page persists.
    pub fn saved_files(&self) -> Vec<(&str, &[u8])> {
        let mut saved: Vec<(&str, &[u8])> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.writable)
            .map(|(name, entry)| (name.as_str(), entry.bytes.as_slice()))
            .collect();
        saved.sort_unstable_by_key(|(name, _)| *name);
        saved
    }

    /// Changes to saved files so far. The page compares it with the last value it
    /// persisted, which is cheaper than diffing every save on every chunk.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// The first file matching a `FindFirstFileA` pattern, with its length.
    pub fn find(&self, pattern: &str) -> Option<(String, u32)> {
        let pattern = basename(&normalize(pattern));
        let mut names: Vec<&String> = self.entries.keys().collect();
        names.sort_unstable();
        names.into_iter().find_map(|key| {
            let name = basename(key);
            if wildcard(&pattern, &name) {
                let length = self.entries.get(key).map_or(0, |entry| entry.length);
                Some((name, length))
            } else {
                None
            }
        })
    }

    /// A token for `FindClose`, so the guest's close has something to close.
    pub fn open_search(&mut self) -> u32 {
        let handle = FIRST_SEARCH + self.searches.len() as u32 * 4;
        // A handle is only a token here, but it must be unique while it is open.
        let mut handle = handle;
        while self.searches.contains(&handle) {
            handle += 4;
        }
        self.searches.insert(handle);
        handle
    }

    pub fn close_search(&mut self, handle: u32) -> bool {
        self.searches.remove(&handle)
    }
}

/// `*` matches any run of characters and `?` one character, case-insensitively,
/// which is what a Win32 search pattern means.
fn wildcard(pattern: &str, name: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let name: Vec<char> = name.chars().collect();
    let (mut p, mut n) = (0_usize, 0_usize);
    let (mut star, mut retry) = (None, 0_usize);
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            retry = n;
            p += 1;
        } else if let Some(at) = star {
            retry += 1;
            n = retry;
            p = at + 1;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|character| *character == '*')
}

/// The guest's path with backslashes, in one case.
fn normalize(path: &str) -> String {
    path.replace('/', "\\").to_lowercase()
}

fn basename(path: &str) -> String {
    path.rsplit('\\').next().unwrap_or(path).to_string()
}

pub const SHIMS: &[Binding] = &[
    Binding { dll: KERNEL32, name: "CreateFileA", arg_bytes: 28, shim: create_file },
    Binding { dll: KERNEL32, name: "ReadFile", arg_bytes: 20, shim: read_file },
    Binding { dll: KERNEL32, name: "SetFilePointer", arg_bytes: 16, shim: set_file_pointer },
    Binding { dll: KERNEL32, name: "WriteFile", arg_bytes: 20, shim: write_file },
    Binding { dll: KERNEL32, name: "SetEndOfFile", arg_bytes: 4, shim: set_end_of_file },
    Binding { dll: KERNEL32, name: "FlushFileBuffers", arg_bytes: 4, shim: flush_file_buffers },
    Binding { dll: KERNEL32, name: "DeleteFileA", arg_bytes: 4, shim: delete_file },
    Binding { dll: KERNEL32, name: "FindFirstFileA", arg_bytes: 8, shim: find_first_file },
    Binding { dll: KERNEL32, name: "FindClose", arg_bytes: 4, shim: find_close },
];

fn create_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = call.cstr(0)?;
    let access = call.arg(1)?;
    let creation = call.arg(4)?;

    let wants_write = access & (GENERIC_WRITE | FILE_WRITE_DATA) != 0;
    let wants_truncate = matches!(creation, CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING);

    if wants_write || wants_truncate {
        // A writable open is a save: a new file of the guest's own, or one it
        // wrote in an earlier session. The bundle stays read-only, and asking to
        // overwrite it is refused rather than silently redirected.
        if let Some(handle) = host.files.create(&path, creation) {
            return Ok(Return(handle));
        }
        if host.files.saved(&path) || !host.files.exists(&path) {
            host.last_error = 87; // ERROR_INVALID_PARAMETER
        } else {
            host.last_error = ERROR_ACCESS_DENIED;
            host.notice(format!(
                "CreateFileA {path}: denied, the game files are read-only"
            ));
        }
        return Ok(Return(INVALID_HANDLE_VALUE));
    }
    if creation != OPEN_EXISTING && creation != OPEN_ALWAYS {
        host.last_error = 87; // ERROR_INVALID_PARAMETER
        return Ok(Return(INVALID_HANDLE_VALUE));
    }

    match host.files.open(&path) {
        Some(handle) => Ok(Return(handle)),
        None => {
            host.last_error = ERROR_FILE_NOT_FOUND;
            Ok(Return(INVALID_HANDLE_VALUE))
        }
    }
}

/// `WriteFile(handle, buffer, bytes, &written, overlapped)`.
fn write_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let buffer = call.arg(1)?;
    let count = call.arg(2)?;
    let written = call.arg(3)?;
    let bytes = if count == 0 {
        Vec::new()
    } else {
        call.memory.read_bytes(buffer, count as usize)?.to_vec()
    };
    match host.files.write(handle, &bytes) {
        Some(count) => {
            if written != 0 {
                call.write_u32(written, count)?;
            }
            Ok(Return(1))
        }
        None => {
            if written != 0 {
                call.write_u32(written, 0)?;
            }
            host.last_error = ERROR_ACCESS_DENIED;
            Ok(Return(0))
        }
    }
}

fn set_end_of_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if host.files.end_of_file(call.arg(0)?) {
        return Ok(Return(1));
    }
    host.last_error = ERROR_ACCESS_DENIED;
    Ok(Return(0))
}

fn flush_file_buffers(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The bytes are already where the page reads them from; there is no device
    // buffer between the guest and its file.
    if host.files.is_open(call.arg(0)?) {
        return Ok(Return(1));
    }
    host.last_error = ERROR_INVALID_HANDLE;
    Ok(Return(0))
}

fn delete_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = call.cstr(0)?;
    if host.files.delete(&path) {
        return Ok(Return(1));
    }
    host.last_error = if host.files.exists(&path) {
        ERROR_ACCESS_DENIED
    } else {
        ERROR_FILE_NOT_FOUND
    };
    Ok(Return(0))
}

/// `FindFirstFileA(pattern, &find_data)`: the first name in the table matching a
/// wildcard pattern. Only the existence check and the name are used by this
/// image -- it opens the file it just found -- so the timestamps are a constant
/// and the size is the entry's.
fn find_first_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let pattern = call.cstr(0)?;
    let destination = call.arg(1)?;
    if destination == 0 {
        host.last_error = 87;
        return Ok(Return(INVALID_HANDLE_VALUE));
    }
    let Some((name, length)) = host.files.find(&pattern) else {
        host.last_error = ERROR_FILE_NOT_FOUND;
        return Ok(Return(INVALID_HANDLE_VALUE));
    };
    call.zero(destination, FIND_DATA_SIZE)?;
    // FILE_ATTRIBUTE_NORMAL, a fixed 2020-01-01 timestamp, and the file size.
    call.write_u32(destination, 0x80)?;
    for offset in [4, 12, 20] {
        call.write_u32(destination + offset, 0xD2B0_9E00)?;
        call.write_u32(destination + offset + 4, 0x01D5_CC00)?;
    }
    call.write_u32(destination + 28, 0)?;
    call.write_u32(destination + 32, length)?;
    call.write(destination + 44, name.as_bytes())?;
    Ok(Return(host.files.open_search()))
}

fn find_close(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if host.files.close_search(call.arg(0)?) {
        return Ok(Return(1));
    }
    host.last_error = ERROR_INVALID_HANDLE;
    Ok(Return(0))
}

fn read_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let destination = call.arg(1)?;
    let requested = call.arg(2)?;
    let copied_out = call.arg(3)?;

    let Some((entry, position)) = host.files.file(handle) else {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(Return(0));
    };
    // Short reads are legal: the guest reads in chunks and stops at zero.
    let available = entry.length.saturating_sub(position);
    let count = requested.min(available);
    if entry.bytes.is_empty() && count > 0 {
        // A synthetic entry, so the bytes are zeroes. The first time that happens
        // is worth saying out loud: the guest was supposed to measure this file,
        // not read it.
        if host.files.synthetic_reads == 0 {
            host.notice("read from a synthetic CD marker: the guest wanted its contents".to_string());
        }
        host.files.synthetic_reads += 1;
        call.zero(destination, count)?;
    } else {
        let chunk = entry.bytes[position as usize..position as usize + count as usize].to_vec();
        call.write(destination, &chunk)?;
    }
    if copied_out != 0 {
        call.write_u32(copied_out, count)?;
    }
    host.files.seek(handle, position.saturating_add(count));
    host.files.bytes_read += u64::from(count);
    Ok(Return(1))
}

fn set_file_pointer(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let distance = call.arg(1)? as i32 as i64;
    let high = call.arg(2)?;
    let method = call.arg(3)?;

    if high != 0 && call.memory.read_u32(high)? != 0 {
        // 64-bit offsets are outside what this host models; the guest is a 32-bit
        // process and this is the truthful failure.
        host.last_error = 87; // ERROR_INVALID_PARAMETER
        return Ok(Return(INVALID_SET_FILE_POINTER));
    }
    let Some((entry, position)) = host.files.file(handle) else {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(Return(INVALID_SET_FILE_POINTER));
    };
    let bytes = entry.length;
    let base = match method {
        FILE_BEGIN => 0_i64,
        FILE_CURRENT => i64::from(position),
        FILE_END => i64::from(bytes),
        _ => {
            host.last_error = 87;
            return Ok(Return(INVALID_SET_FILE_POINTER));
        }
    };
    let moved = base + distance;
    if moved < 0 || moved > i64::from(u32::MAX) {
        host.last_error = 87;
        return Ok(Return(INVALID_SET_FILE_POINTER));
    }
    host.files.seek(handle, moved as u32);
    Ok(Return(moved as u32))
}

/// `CloseHandle` for a file, and the answer `GetFileType`/`WriteFile` owe it. Both
/// live here because the file table owns the handles; `kernel32` delegates for
/// anything else.
pub(crate) fn close(host: &mut Host, handle: u32) -> bool {
    host.files.close(handle)
}

pub(crate) fn file_type(host: &Host, handle: u32) -> Option<u32> {
    host.files.file(handle).map(|_| FILE_TYPE_DISK)
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
        let args = STACK_BASE + 0x8000;
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

    /// Opens `sfx.dat` the way the game does and returns the handle.
    fn open_sfx(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        let name = STACK_BASE + 0x9000;
        memory.write_bytes(name, b"sfx.dat\0").unwrap();
        let action = call_shim(
            host,
            memory,
            "CreateFileA",
            &[name, 0x8000_0000, 3, 0, OPEN_EXISTING, 0x80, 0],
        )
        .unwrap();
        let Return(handle) = action else {
            panic!("CreateFileA returned {action:?}");
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "the file is provided");
        handle
    }

    #[test]
    fn a_provided_file_opens_by_basename_whatever_the_path() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1, 2, 3, 4]);
        let path = STACK_BASE + 0x9100;
        memory.write_bytes(path, b"C:\\Games\\SFX.DAT\0").unwrap();
        let action = call_shim(
            &mut host,
            &mut memory,
            "CreateFileA",
            &[path, 0x8000_0000, 3, 0, OPEN_EXISTING, 0x80, 0],
        )
        .unwrap();
        let Return(handle) = action else {
            panic!("expected a handle, got {action:?}");
        };
        assert!(handle >= FIRST_FILE);
    }

    #[test]
    fn reading_advances_the_cursor_and_stops_at_the_end() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![10, 20, 30, 40, 50]);
        let handle = open_sfx(&mut host, &mut memory);
        let buffer = STACK_BASE + 0x9200;
        let copied = STACK_BASE + 0x9300;

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFile",
                &[handle, buffer, 2, copied, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(copied).unwrap(), 2);
        assert_eq!(memory.read_bytes(buffer, 2).unwrap(), &[10, 20]);

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFile",
                &[handle, buffer, 8, copied, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(copied).unwrap(), 3, "a short read, not a failure");
        assert_eq!(memory.read_bytes(buffer, 3).unwrap(), &[30, 40, 50]);

        // At the end, success with zero bytes is what the CRT expects.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFile",
                &[handle, buffer, 8, copied, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(copied).unwrap(), 0);
    }

    #[test]
    fn seeking_from_the_beginning_current_and_end() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1, 2, 3, 4, 5, 6]);
        let handle = open_sfx(&mut host, &mut memory);
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetFilePointer",
                &[handle, 4, 0, FILE_BEGIN]
            )
            .unwrap(),
            Return(4)
        );
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetFilePointer",
                &[handle, 1, 0, FILE_CURRENT]
            )
            .unwrap(),
            Return(5)
        );
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetFilePointer",
                &[handle, (-2_i32) as u32, 0, FILE_END]
            )
            .unwrap(),
            Return(4)
        );
        // Below zero is a bad argument, not a wrapped huge offset.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "SetFilePointer",
                &[handle, (-9_i32) as u32, 0, FILE_END]
            )
            .unwrap(),
            Return(INVALID_SET_FILE_POINTER)
        );
        assert_eq!(host.last_error, 87);
    }

    #[test]
    fn a_file_that_was_not_provided_is_file_not_found() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let name = STACK_BASE + 0x9500;
        memory.write_bytes(name, b"missing.dat\0").unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "CreateFileA",
                &[name, 0x8000_0000, 3, 0, OPEN_EXISTING, 0x80, 0]
            )
            .unwrap(),
            Return(INVALID_HANDLE_VALUE)
        );
        assert_eq!(host.last_error, ERROR_FILE_NOT_FOUND);
    }

    #[test]
    fn a_bundle_file_cannot_be_overwritten() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("save.dat", vec![0; 4]);
        let name = STACK_BASE + 0x9600;
        memory.write_bytes(name, b"save.dat\0").unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "CreateFileA",
                &[name, 0x4000_0000, 0, 0, CREATE_ALWAYS, 0x80, 0]
            )
            .unwrap(),
            Return(INVALID_HANDLE_VALUE)
        );
        assert_eq!(host.last_error, ERROR_ACCESS_DENIED);
        assert_eq!(host.notices.len(), 1);
        assert!(host.notices[0].contains("read-only"));
    }

    /// Opens `name` for writing and returns the handle.
    fn create_save(host: &mut Host, memory: &mut GuestMemory, name_bytes: &[u8]) -> u32 {
        let name = STACK_BASE + 0x9a00;
        memory.write_bytes(name, name_bytes).unwrap();
        let action = call_shim(
            host,
            memory,
            "CreateFileA",
            &[name, 0xc000_0000, 0, 0, CREATE_ALWAYS, 0x80, 0],
        )
        .unwrap();
        let Return(handle) = action else {
            panic!("CreateFileA returned {action:?}");
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "a new save opens");
        handle
    }

    #[test]
    fn a_new_file_is_writable_and_comes_back_as_a_save() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let before = host.files.version();
        let handle = create_save(&mut host, &mut memory, b"C:\\Games\\save0001.dat\0");
        let buffer = STACK_BASE + 0x9b00;
        let written = STACK_BASE + 0x9c00;
        memory.write_bytes(buffer, b"hello save").unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "WriteFile",
                &[handle, buffer, 10, written, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(written).unwrap(), 10);
        assert!(host.files.version() > before, "the page learns there is a save");
        assert_eq!(host.files.saved_files(), vec![("c:\\games\\save0001.dat", &b"hello save"[..])]);
        assert_eq!(host.files.count(), 1);
    }

    #[test]
    fn a_save_reads_back_what_was_written_and_can_be_seeked() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = create_save(&mut host, &mut memory, b"save0002.dat\0");
        let buffer = STACK_BASE + 0x9d00;
        let written = STACK_BASE + 0x9e00;
        memory.write_bytes(buffer, &[1, 2, 3, 4]).unwrap();
        call_shim(&mut host, &mut memory, "WriteFile", &[handle, buffer, 4, written, 0]).unwrap();
        // Closing and opening again is what the game does on the next screen.
        assert!(close(&mut host, handle));
        let name = STACK_BASE + 0x9f00;
        memory.write_bytes(name, b"SAVE0002.DAT\0").unwrap();
        let action = call_shim(
            &mut host,
            &mut memory,
            "CreateFileA",
            &[name, 0x8000_0000, 3, 0, OPEN_EXISTING, 0x80, 0],
        )
        .unwrap();
        let Return(handle) = action else {
            panic!("CreateFileA returned {action:?}");
        };
        let read = STACK_BASE + 0xa000;
        let copied = STACK_BASE + 0xa100;
        assert_eq!(
            call_shim(&mut host, &mut memory, "ReadFile", &[handle, read, 4, copied, 0]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_bytes(read, 4).unwrap(), &[1, 2, 3, 4]);
    }

    #[test]
    fn set_end_of_file_truncates_at_the_cursor() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = create_save(&mut host, &mut memory, b"save0003.dat\0");
        let buffer = STACK_BASE + 0xa200;
        let written = STACK_BASE + 0xa300;
        memory.write_bytes(buffer, b"0123456789").unwrap();
        call_shim(&mut host, &mut memory, "WriteFile", &[handle, buffer, 10, written, 0]).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "SetFilePointer", &[handle, 4, 0, FILE_BEGIN])
                .unwrap(),
            Return(4)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "SetEndOfFile", &[handle]).unwrap(),
            Return(1)
        );
        assert_eq!(host.files.saved_files(), vec![("save0003.dat", &b"0123"[..])]);
    }

    #[test]
    fn a_save_can_be_deleted_and_a_bundle_file_cannot() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1]);
        let save = STACK_BASE + 0xa400;
        memory.write_bytes(save, b"save0004.dat\0").unwrap();
        create_save(&mut host, &mut memory, b"save0004.dat\0");
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteFileA", &[save]).unwrap(),
            Return(1)
        );
        assert!(!host.files.exists("save0004.dat"));

        let bundle = STACK_BASE + 0xa500;
        memory.write_bytes(bundle, b"sfx.dat\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteFileA", &[bundle]).unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_ACCESS_DENIED);
        assert!(host.files.exists("sfx.dat"));
    }

    #[test]
    fn find_first_file_reports_a_save_the_guest_can_then_open() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1, 2]);
        create_save(&mut host, &mut memory, b"save0005.dat\0");
        let pattern = STACK_BASE + 0xa600;
        let data = STACK_BASE + 0xa700;
        memory.write_bytes(pattern, b"save000*.dat\0").unwrap();
        let action = call_shim(&mut host, &mut memory, "FindFirstFileA", &[pattern, data]).unwrap();
        let Return(search) = action else {
            panic!("FindFirstFileA returned {action:?}");
        };
        assert_ne!(search, INVALID_HANDLE_VALUE);
        let name = memory.read_bytes(data + 44, 12).unwrap();
        assert_eq!(name, b"save0005.dat");
        assert_eq!(memory.read_u32(data + 32).unwrap(), 0, "an empty file");
        assert_eq!(
            call_shim(&mut host, &mut memory, "FindClose", &[search]).unwrap(),
            Return(1)
        );

        // A pattern nothing matches is "no file", which is how the guest decides
        // that a slot is empty.
        memory.write_bytes(pattern, b"save0099.dat\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "FindFirstFileA", &[pattern, data]).unwrap(),
            Return(INVALID_HANDLE_VALUE)
        );
        assert_eq!(host.last_error, ERROR_FILE_NOT_FOUND);
    }

    #[test]
    fn a_provided_save_is_writable_and_the_bundle_is_not() {
        let mut files = Files::default();
        files.provide("sfx.dat", vec![1]);
        files.provide_saved("save0001.dat", vec![2, 3]);
        assert!(files.saved("save0001.dat"));
        assert!(!files.saved("sfx.dat"));
        assert_eq!(files.version(), 0, "a restored save is not a change");
        assert!(
            files.create("save0001.dat", CREATE_ALWAYS).is_some(),
            "a restored save may be overwritten"
        );
        assert_eq!(
            files.create("sfx.dat", CREATE_ALWAYS),
            None,
            "a bundle file may not"
        );
    }

    #[test]
    fn a_closed_handle_cannot_be_read() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1]);
        let handle = open_sfx(&mut host, &mut memory);
        assert!(close(&mut host, handle));
        assert!(!close(&mut host, handle), "a second close is not a handle");
        let copied = STACK_BASE + 0x9700;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFile",
                &[handle, STACK_BASE + 0x9800, 1, copied, 0]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_INVALID_HANDLE);
    }

    #[test]
    fn a_file_handle_is_a_disk_not_a_console() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1]);
        let handle = open_sfx(&mut host, &mut memory);
        assert_eq!(file_type(&host, handle), Some(FILE_TYPE_DISK));
        assert_eq!(file_type(&host, 0x11), None, "standard input is not a file");
    }

    #[test]
    fn the_table_reports_what_it_holds() {
        let mut files = Files::default();
        assert!(files.is_empty());
        files.provide("Data\\sfx.dat", vec![0; 10]);
        files.provide("BGM.dat", vec![0; 5]);
        assert_eq!(files.count(), 2);
        assert_eq!(files.total_bytes(), 15);
        assert_eq!(files.names(), vec!["bgm.dat", "data\\sfx.dat"]);
    }
}
