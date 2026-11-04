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
//! # A folder the guest owns
//!
//! The payload is a directory, and on Windows a game's directory is writable:
//! the game rewrites `data\settings.dat` on start and drops `save0001.dat`
//! beside its executable. So every name here is writable. What the guest writes
//! lands in [`Files::saved`] and the payload's bytes stay as delivered in
//! [`Files::install`]; a read prefers the guest's copy, which is what makes the
//! install behave like the folder it stands in for. Nothing the guest does can
//! reach the bytes the page fetched, so a reload starts from the payload exactly
//! as it was installed.
//!
//! Two things separate this from a folder on a disk. Writability belongs to the
//! **handle**, not the file: a read-only `CreateFileA` cannot `WriteFile`, even
//! while another handle has the same file open for writing. And the guest's copy
//! is the only thing that outlives a session: the page copies
//! [`Files::saved_files`] out through the `wh_saved_*` exports, keeps it in
//! browser storage, and hands it back with [`Files::provide_saved`] before the
//! guest starts -- in either order, because a save shadows the install file
//! instead of replacing it. No path is opened on the machine, so a page served
//! over the network writes nothing anywhere but its own storage.
use std::collections::{BTreeSet, HashMap};

use crate::shim::{Action, Binding, Call, FileApc, Host};
use crate::Result;

use Action::Return;

const KERNEL32: &str = "KERNEL32.dll";

const INVALID_HANDLE_VALUE: u32 = 0xffff_ffff;
const INVALID_SET_FILE_POINTER: u32 = 0xffff_ffff;

const ERROR_FILE_NOT_FOUND: u32 = 2;
const ERROR_ACCESS_DENIED: u32 = 5;
const ERROR_FILE_EXISTS: u32 = 80;
const ERROR_INVALID_HANDLE: u32 = 6;
const ERROR_IO_PENDING: u32 = 997;

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

/// `WIN32_FIND_DATAW`: the same 44-byte header, then 520 bytes of UTF-16 name and
/// 28 of the short name.
const FIND_DATA_W_SIZE: u32 = 0x250;

/// A file the guest has open: which table its bytes are in, and its cursor.
struct Open {
    key: String,
    /// The payload's table, or the guest's own.
    table: Where,
    /// Whether *this handle* may write. Windows puts the right on the handle,
    /// not on the file, so a read-only open stays read-only even while another
    /// handle has the same file open for writing.
    writable: bool,
    position: u32,
}

/// Which table a resolved name lives in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Where {
    /// The payload as delivered. Never written.
    Install,
    /// The guest's own copy, which shadows the install file of the same name.
    Saved,
}

/// One file's bytes.
///
/// A **synthetic** entry has a length and no bytes: every read returns zeroes.
/// It exists for the one file the guest only ever measures -- the CD's
/// `data1.cab`, which it opens and compares against 150,000,000 bytes to decide
/// that the disc is in the drive. Handing it 150 MB of real zeroes would cost the
/// page 150 MB for a number.
struct Entry {
    bytes: Vec<u8>,
    length: u32,
}

impl Entry {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            length: bytes.len() as u32,
            bytes,
        }
    }
}

/// Every byte the guest can read, every byte it has written, and the handles
/// open on either.
#[derive(Default)]
pub struct Files {
    /// The payload as delivered, by lowercased path.
    install: HashMap<String, Entry>,
    /// Basename, lowercased, to the install key. The guest builds its own paths.
    install_by_name: HashMap<String, String>,
    /// What the guest has written, which is the folder it owns: a name it opens
    /// for writing lands here, bytes and all, and every later read of that name
    /// finds this copy first.
    saved: HashMap<String, Entry>,
    saved_by_name: HashMap<String, String>,
    /// Basenames the guest deleted, so a payload file it removed does not come
    /// back through the install table on the next read.
    removed: BTreeSet<String>,
    open: HashMap<u32, Open>,
    next_handle: u32,
    /// Handles `FindFirstFileA` handed out, so `FindClose` has something to
    /// close, and the pattern and cursor behind each one so `FindNextFileA` can
    /// continue where the previous call stopped.
    searches: BTreeSet<u32>,
    search_at: HashMap<u32, (String, usize)>,
    /// Bumped whenever a file the guest owns changes, which is how the page
    /// knows there is something to persist without being told what a save is.
    version: u64,
    /// Bytes this table has handed to the guest, which is how a session test can
    /// tell "the guest opened a file" from "the guest read one".
    bytes_read: u64,
    /// Reads served from a synthetic entry, which is worth exactly one notice:
    /// the guest measuring a marker is expected, the guest *reading* one is not.
    synthetic_reads: u32,
}

impl Files {
    /// Adds the payload's copy of a file. It is what the guest reads until the
    /// guest writes that name, and it is never written itself.
    pub fn provide(&mut self, path: &str, bytes: Vec<u8>) {
        let key = normalize(path);
        let base = basename(&key);
        self.install_by_name.insert(base, key.clone());
        self.install.insert(key, Entry::new(bytes));
    }

    /// Adds a file the guest saved in an earlier session, which reads back as
    /// the guest's own from the moment it is here.
    pub fn provide_saved(&mut self, path: &str, bytes: Vec<u8>) {
        let key = normalize(path);
        let base = basename(&key);
        self.saved_by_name.insert(base, key.clone());
        self.saved.insert(key, Entry::new(bytes));
    }

    /// Provides a file of `length` zero bytes that costs nothing to hold.
    pub fn provide_sized(&mut self, path: &str, length: u32) {
        let key = normalize(path);
        let base = basename(&key);
        self.install_by_name.insert(base, key.clone());
        self.install.insert(
            key,
            Entry {
                bytes: Vec::new(),
                length,
            },
        );
    }

    /// Whether the entry behind `path` is synthetic, which the session tests use
    /// to show a marker is a marker.
    pub fn synthetic(&self, path: &str) -> bool {
        self.resolve(path)
            .and_then(|(table, key)| self.entry(table, &key))
            .is_some_and(|entry| entry.bytes.is_empty() && entry.length > 0)
    }

    pub fn count(&self) -> usize {
        self.install.len() + self.saved.len()
    }

    pub fn total_bytes(&self) -> usize {
        self.install
            .values()
            .chain(self.saved.values())
            .map(|entry| entry.bytes.len())
            .sum()
    }

    /// Bytes the guest could read if it wanted it all.
    pub fn total_length(&self) -> u64 {
        self.install
            .values()
            .chain(self.saved.values())
            .map(|entry| u64::from(entry.length))
            .sum()
    }

    /// Every provided name, in the order a directory listing would want them.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = self
            .install
            .keys()
            .chain(self.saved.keys())
            .filter(|key| !self.removed.contains(&basename(key)))
            .map(String::as_str)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    pub fn is_empty(&self) -> bool {
        self.install.is_empty() && self.saved.is_empty()
    }

    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }

    /// Where a name reads from: the guest's own copy if it has one, otherwise
    /// the payload's, and nothing at all if the guest deleted it.
    fn resolve(&self, path: &str) -> Option<(Where, String)> {
        let key = normalize(path);
        let base = basename(&key);
        if self.removed.contains(&base) {
            return None;
        }
        if self.saved.contains_key(&key) {
            return Some((Where::Saved, key));
        }
        if let Some(saved) = self.saved_by_name.get(&base) {
            return Some((Where::Saved, saved.clone()));
        }
        if self.install.contains_key(&key) {
            return Some((Where::Install, key));
        }
        self.install_by_name
            .get(&base)
            .cloned()
            .map(|key| (Where::Install, key))
    }

    fn entry(&self, table: Where, key: &str) -> Option<&Entry> {
        match table {
            Where::Install => self.install.get(key),
            Where::Saved => self.saved.get(key),
        }
    }

    /// The whole contents of a file.
    ///
    /// A loader that takes a *path* rather than a handle -- `LoadImageA` with
    /// `LR_LOADFROMFILE` -- has no cursor to advance, so it wants the bytes in
    /// one piece instead of a handle.
    pub fn contents(&self, path: &str) -> Option<&[u8]> {
        let (table, key) = self.resolve(path)?;
        self.entry(table, &key).map(|entry| entry.bytes.as_slice())
    }

    /// The whole contents of an open file, which `CreateFileMappingA` maps: the
    /// guest may hand the mapping a handle rather than a path.
    pub fn open_contents(&self, handle: u32) -> Option<Vec<u8>> {
        let (entry, _) = self.file(handle)?;
        Some(entry.bytes.clone())
    }

    /// Reads up to `count` bytes at the cursor and advances it, which is what a
    /// `FILE` stream reads through. A supplied marker -- a name with a length and
    /// no bytes -- reads as zeroes, exactly as `ReadFile` answers it.
    pub fn read(&mut self, handle: u32, count: u32) -> Option<Vec<u8>> {
        let (entry, position) = self.file(handle)?;
        let take = count.min(entry.length.saturating_sub(position));
        let bytes = if entry.bytes.is_empty() {
            vec![0; take as usize]
        } else {
            entry.bytes[position as usize..(position + take) as usize].to_vec()
        };
        self.seek(handle, position.saturating_add(take));
        self.bytes_read += u64::from(take);
        Some(bytes)
    }

    /// Opens a file for reading and returns its handle.
    pub fn open(&mut self, path: &str) -> Option<u32> {
        let (table, key) = self.resolve(path)?;
        let handle = FIRST_FILE + self.next_handle * 4;
        self.next_handle += 1;
        self.open.insert(
            handle,
            Open {
                key,
                table,
                writable: false,
                position: 0,
            },
        );
        Some(handle)
    }

    pub fn close(&mut self, handle: u32) -> bool {
        self.open.remove(&handle).is_some()
    }

    /// The handle's file, or `None` if the guest never opened it.
    fn file(&self, handle: u32) -> Option<(&Entry, u32)> {
        let open = self.open.get(&handle)?;
        Some((self.entry(open.table, &open.key)?, open.position))
    }

    pub fn seek(&mut self, handle: u32, position: u32) {
        if let Some(open) = self.open.get_mut(&handle) {
            open.position = position;
        }
    }

    /// The open file's cursor, which `SetFilePointerEx` reports back.
    pub fn position(&self, handle: u32) -> Option<u32> {
        self.open.get(&handle).map(|open| open.position)
    }

    /// The open file's length, which is where `FILE_END` resolves from.
    pub fn length(&self, handle: u32) -> Option<u32> {
        self.file(handle).map(|(entry, _)| entry.length)
    }

    /// Whether the table holds `path` at all, which separates "denied" from
    /// "not found".
    pub fn exists(&self, path: &str) -> bool {
        self.resolve(path).is_some()
    }

    /// Whether a handle is open, for the calls that only need a valid handle.
    pub fn is_open(&self, handle: u32) -> bool {
        self.open.contains_key(&handle)
    }

    /// Whether the guest has its own copy of `path`, which is what the page
    /// persists.
    pub fn saved(&self, path: &str) -> bool {
        matches!(self.resolve(path), Some((Where::Saved, _)))
    }

    /// Opens `path` for writing, creating or truncating it as `creation` asks.
    /// Returns `None` when the request contradicts what the table holds -- a
    /// `CREATE_NEW` on a name that is already there, or an open of a file that
    /// is not -- so the caller can answer with the right Win32 error.
    pub fn create(&mut self, path: &str, creation: u32) -> Option<u32> {
        const CREATE_NEW: u32 = 1;
        const CREATE_ALWAYS: u32 = 2;
        const OPEN_EXISTING: u32 = 3;
        const TRUNCATE_EXISTING: u32 = 5;

        let wanted = normalize(path);
        let base = basename(&wanted);
        if creation == CREATE_NEW && self.resolve(path).is_some() {
            // The file is already there, and `CREATE_NEW` asked for a new one --
            // whether the payload supplied it or the guest wrote it.
            return None;
        }
        let key = match self.resolve(path) {
            Some((Where::Saved, key)) => key,
            Some((Where::Install, key)) => {
                // The payload's copy stays as delivered: the guest's writes go
                // into a copy of it, which shadows it for every read. That is
                // what makes the install a folder the guest owns rather than one
                // it can only look at.
                let entry = self.install.get(&key)?;
                let copy = Entry {
                    bytes: entry.bytes.clone(),
                    length: entry.length,
                };
                self.saved_by_name.insert(base.clone(), wanted.clone());
                self.saved.insert(wanted.clone(), copy);
                self.version += 1;
                wanted
            }
            None => {
                if matches!(creation, OPEN_EXISTING | TRUNCATE_EXISTING) {
                    return None;
                }
                self.saved_by_name.insert(base.clone(), wanted.clone());
                self.saved.insert(wanted.clone(), Entry::new(Vec::new()));
                self.version += 1;
                wanted
            }
        };
        // A file that is being created is there again, so a delete of that name
        // is over.
        self.removed.remove(&base);
        if matches!(creation, CREATE_ALWAYS | TRUNCATE_EXISTING) {
            if let Some(entry) = self.saved.get_mut(&key) {
                entry.bytes.clear();
                entry.length = 0;
                self.version += 1;
            }
        }
        let handle = FIRST_FILE + self.next_handle * 4;
        self.next_handle += 1;
        self.open.insert(
            handle,
            Open {
                key,
                table: Where::Saved,
                writable: true,
                position: 0,
            },
        );
        Some(handle)
    }

    /// Writes at the open file's cursor and advances it, growing the file as a
    /// real one grows. `None` when the handle was not opened for writing.
    pub fn write(&mut self, handle: u32, bytes: &[u8]) -> Option<u32> {
        let Self { open, saved, .. } = self;
        let open = open.get_mut(&handle)?;
        if !open.writable {
            return None;
        }
        let entry = saved.get_mut(&open.key)?;
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
        let Self { open, saved, .. } = self;
        let Some(open) = open.get(&handle) else {
            return false;
        };
        if !open.writable {
            return false;
        }
        let position = open.position as usize;
        let Some(entry) = saved.get_mut(&open.key) else {
            return false;
        };
        entry.bytes.resize(position, 0);
        entry.length = position as u32;
        self.version += 1;
        true
    }

    /// `DeleteFileA`. The payload's files are the guest's to remove too: a folder
    /// it can write is a folder it can delete from. The install bytes stay for
    /// the next session, so a delete here hides a file rather than destroying it.
    pub fn delete(&mut self, path: &str) -> bool {
        let Some((table, key)) = self.resolve(path) else {
            return false;
        };
        self.removed.insert(basename(&key));
        if table == Where::Saved {
            self.saved.remove(&key);
            self.saved_by_name.retain(|_, entry| *entry != key);
        }
        self.open.retain(|_, open| open.key != key);
        self.version += 1;
        true
    }

    /// The files the guest wrote, by name, which is what the page persists.
    pub fn saved_files(&self) -> Vec<(&str, &[u8])> {
        let mut saved: Vec<(&str, &[u8])> = self
            .saved
            .iter()
            .map(|(name, entry)| (name.as_str(), entry.bytes.as_slice()))
            .collect();
        saved.sort_unstable_by_key(|(name, _)| *name);
        saved
    }

    /// Changes to the guest's files so far. The page compares it with the last
    /// value it persisted, which is cheaper than diffing every save on every
    /// chunk.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Every file matching a `FindFirstFileA` pattern, in the order Windows
    /// enumerates a directory: by name.
    ///
    /// The pattern's directory is part of the match, so `sprite\Loading\*`
    /// answers with the files in that folder and not with every file of that
    /// shape in the payload. A leading `.` is the working directory, which is
    /// the payload's root. A pattern nothing matches as a whole path is tried as
    /// a bare name, which is what a caller that glued a path of its own onto the
    /// name expects.
    fn matches(&self, pattern: &str) -> Vec<(String, u32)> {
        let pattern = normalize(pattern);
        let pattern = pattern.strip_prefix(".\\").unwrap_or(&pattern).to_string();
        let mut found = self.matched(&pattern, true);
        if found.is_empty() {
            found = self.matched(&basename(&pattern), false);
        }
        found
    }

    /// The matches of one wildcard, against the whole key when `path` is set and
    /// against each entry's name when it is not.
    fn matched(&self, pattern: &str, path: bool) -> Vec<(String, u32)> {
        // A name the guest wrote wins over the payload's copy, length and all.
        let mut found: HashMap<String, u32> = HashMap::new();
        // A Win32 search reports the two directory entries of the folder it
        // looked in, and a guest that walks a listing counts on them: this
        // game's loading screen throws its first hit away as `.` before it
        // collects the pictures.
        if wildcard(&basename(pattern), ".") {
            found.insert(".".to_string(), 0);
            found.insert("..".to_string(), 0);
        }
        for (key, entry) in self.install.iter().chain(self.saved.iter()) {
            let name = basename(key);
            if self.removed.contains(&name) {
                continue;
            }
            let subject = if path { normalize(key) } else { name.clone() };
            if wildcard(pattern, &subject) {
                found.insert(name, entry.length);
            }
        }
        let mut found: Vec<(String, u32)> = found.into_iter().collect();
        found.sort_unstable_by(|left, right| left.0.cmp(&right.0));
        found
    }

    /// The first file matching a `FindFirstFileA` pattern, with its length.
    pub fn find(&self, pattern: &str) -> Option<(String, u32)> {
        self.matches(pattern).into_iter().next()
    }

    /// A token for `FindClose`, remembering the pattern and the position after
    /// the match `FindFirstFileA` already returned.
    pub fn open_search(&mut self, pattern: &str) -> u32 {
        let mut handle = FIRST_SEARCH + self.searches.len() as u32 * 4;
        while self.searches.contains(&handle) {
            handle += 4;
        }
        self.searches.insert(handle);
        self.search_at
            .insert(handle, (normalize(pattern), 1));
        handle
    }

    /// The next match after this search's cursor, advancing it.
    pub fn find_next(&mut self, handle: u32) -> Option<(String, u32)> {
        let (pattern, index) = self.search_at.get(&handle).cloned()?;
        let matches = self.matches(&pattern);
        let next = matches.get(index).cloned()?;
        self.search_at.insert(handle, (pattern, index + 1));
        Some(next)
    }

    pub fn close_search(&mut self, handle: u32) -> bool {
        self.search_at.remove(&handle);
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

/// Whether this search result is one of the folder's own entries.
fn directory(name: &str) -> bool {
    name == "." || name == ".."
}

fn basename(path: &str) -> String {
    path.rsplit('\\').next().unwrap_or(path).to_string()
}

pub const SHIMS: &[Binding] = &[
    Binding { dll: KERNEL32, name: "CreateFileA", arg_bytes: 28, shim: create_file },
    Binding { dll: KERNEL32, name: "ReadFile", arg_bytes: 20, shim: read_file },
    Binding { dll: KERNEL32, name: "ReadFileEx", arg_bytes: 20, shim: read_file_ex },
    Binding { dll: KERNEL32, name: "SetFilePointer", arg_bytes: 16, shim: set_file_pointer },
    Binding { dll: KERNEL32, name: "WriteFile", arg_bytes: 20, shim: write_file },
    Binding { dll: KERNEL32, name: "SetEndOfFile", arg_bytes: 4, shim: set_end_of_file },
    Binding { dll: KERNEL32, name: "FlushFileBuffers", arg_bytes: 4, shim: flush_file_buffers },
    Binding { dll: KERNEL32, name: "DeleteFileA", arg_bytes: 4, shim: delete_file },
    Binding { dll: KERNEL32, name: "FindFirstFileA", arg_bytes: 8, shim: find_first_file },
    Binding { dll: KERNEL32, name: "FindFirstFileW", arg_bytes: 8, shim: find_first_file_w },
    Binding { dll: KERNEL32, name: "FindClose", arg_bytes: 4, shim: find_close },
];

fn create_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = call.cstr(0)?;
    Ok(Return(open_path(host, &path, call.arg(1)?, call.arg(4)?)))
}

/// The open `CreateFileA` and `CreateFileW` share, once their name has been
/// decoded. Every name in the folder is writable; `access` decides what *this
/// handle* may do with it, which is what `WriteFile` checks.
pub(crate) fn open_path(host: &mut Host, path: &str, access: u32, creation: u32) -> u32 {
    let wants_write = access & (GENERIC_WRITE | FILE_WRITE_DATA) != 0;
    let wants_truncate = matches!(creation, CREATE_NEW | CREATE_ALWAYS | TRUNCATE_EXISTING);

    if wants_write || wants_truncate {
        if let Some(handle) = host.files.create(path, creation) {
            return handle;
        }
        // The only contradictions left: `CREATE_NEW` on a file that is already
        // there, or a writable open of a file that does not exist.
        host.last_error = if host.files.exists(path) {
            ERROR_FILE_EXISTS
        } else {
            ERROR_FILE_NOT_FOUND
        };
        return INVALID_HANDLE_VALUE;
    }
    if creation != OPEN_EXISTING && creation != OPEN_ALWAYS {
        host.last_error = 87; // ERROR_INVALID_PARAMETER
        return INVALID_HANDLE_VALUE;
    }

    match host.files.open(path) {
        Some(handle) => {
            if std::env::var("SHIMKIT_LOG_FILES").is_ok() && host.notices.len() < 600 {
                host.notice(format!("opened {path}"));
            }
            handle
        }
        None => {
            host.last_error = ERROR_FILE_NOT_FOUND;
            if std::env::var("SHIMKIT_LOG_FILES").is_ok() && host.notices.len() < 400 {
                host.notice(format!("missing {path}"));
            }
            INVALID_HANDLE_VALUE
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
    write_find_data(call, destination, &name, length)?;
    Ok(Return(host.files.open_search(&pattern)))
}

/// `WIN32_FIND_DATAA`, which both find calls write: attributes, a fixed
/// timestamp, the size, and the name in `cFileName`.
fn write_find_data(
    call: &mut Call<'_>,
    destination: u32,
    name: &str,
    length: u32,
) -> Result<()> {
    // `FILE_ATTRIBUTE_ARCHIVE`, which is what Windows reports for an ordinary
    // file. `FILE_ATTRIBUTE_NORMAL` is only an input to `SetFileAttributes`, and
    // a guest that tests for the archive bit saw every file as something else.
    // The two directory entries a search reports say they are directories.
    call.write_u32(destination, if directory(name) { 0x10 } else { 0x20 })?;
    for offset in [4, 12, 20] {
        call.write_u32(destination + offset, 0xD2B0_9E00)?;
        call.write_u32(destination + offset + 4, 0x01D5_CC00)?;
    }
    call.write_u32(destination + 28, 0)?;
    call.write_u32(destination + 32, length)?;
    call.write(destination + 44, name.as_bytes())?;
    Ok(())
}

/// `FindFirstFileW(pattern, &find_data)`: the same search, with a wide pattern
/// and a wide `WIN32_FIND_DATAW`.
fn find_first_file_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let pattern = call.wstr(call.arg(0)?)?;
    let destination = call.arg(1)?;
    if destination == 0 {
        host.last_error = 87;
        return Ok(Return(INVALID_HANDLE_VALUE));
    }
    let Some((name, length)) = host.files.find(&pattern) else {
        host.last_error = ERROR_FILE_NOT_FOUND;
        return Ok(Return(INVALID_HANDLE_VALUE));
    };
    call.zero(destination, FIND_DATA_W_SIZE)?;
    write_find_data_w(call, destination, &name, length)?;
    Ok(Return(host.files.open_search(&pattern)))
}

/// `WIN32_FIND_DATAW`: the same header as the ANSI structure, with the name
/// stored as UTF-16.
fn write_find_data_w(
    call: &mut Call<'_>,
    destination: u32,
    name: &str,
    length: u32,
) -> Result<()> {
    // `FILE_ATTRIBUTE_ARCHIVE`, as in the ANSI structure.
    call.write_u32(destination, if directory(name) { 0x10 } else { 0x20 })?;
    for offset in [4, 12, 20] {
        call.write_u32(destination + offset, 0xD2B0_9E00)?;
        call.write_u32(destination + offset + 4, 0x01D5_CC00)?;
    }
    call.write_u32(destination + 28, 0)?;
    call.write_u32(destination + 32, length)?;
    for (index, unit) in name.encode_utf16().take(259).enumerate() {
        call.write_u16(destination + 44 + index as u32 * 2, unit)?;
    }
    Ok(())
}

/// `FindNextFileW`: continue a wide enumeration.
pub(crate) fn find_next_file_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let destination = call.arg(1)?;
    if destination == 0 {
        host.last_error = 87;
        return Ok(Return(0));
    }
    let Some((name, length)) = host.files.find_next(handle) else {
        host.last_error = 18; // ERROR_NO_MORE_FILES
        return Ok(Return(0));
    };
    call.zero(destination, FIND_DATA_W_SIZE)?;
    write_find_data_w(call, destination, &name, length)?;
    Ok(Return(1))
}

/// `FindNextFileA`: continue the enumeration `FindFirstFileA` started.
pub(crate) fn find_next_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let destination = call.arg(1)?;
    if destination == 0 {
        host.last_error = 87;
        return Ok(Return(0));
    }
    let Some((name, length)) = host.files.find_next(handle) else {
        host.last_error = 18; // ERROR_NO_MORE_FILES
        return Ok(Return(0));
    };
    call.zero(destination, FIND_DATA_SIZE)?;
    write_find_data(call, destination, &name, length)?;
    Ok(Return(1))
}

/// `SetFilePointerEx(handle, distance, &new, method)`: the same cursor
/// `SetFilePointer` moves, with the 64-bit distance passed by value.
pub(crate) fn set_file_pointer_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let distance = call.arg(1)? as i32 as i64;
    let high = call.arg(2)?;
    let destination = call.arg(3)?;
    let method = call.arg(4)?;
    if high != 0 {
        host.last_error = 87; // ERROR_INVALID_PARAMETER
        return Ok(Return(0));
    }
    let Some(position) = host.files.position(handle) else {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(Return(0));
    };
    let length = host.files.length(handle).unwrap_or(0);
    let base = match method {
        FILE_BEGIN => 0_i64,
        FILE_CURRENT => i64::from(position),
        FILE_END => i64::from(length),
        _ => {
            host.last_error = 87;
            return Ok(Return(0));
        }
    };
    let moved = base + distance;
    if moved < 0 || moved > i64::from(u32::MAX) {
        host.last_error = 87;
        return Ok(Return(0));
    }
    host.files.seek(handle, moved as u32);
    if destination != 0 {
        call.write_u32(destination, moved as u32)?;
        call.write_u32(destination + 4, 0)?;
    }
    Ok(Return(1))
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
    let overlapped = call.arg(4)?;

    // An overlapped read takes its offset from the `OVERLAPPED` structure and
    // leaves the file pointer where it was: the guest that posted it is free to
    // seek or post the next buffer while this one is in flight. Reading from the
    // file pointer instead both returns the wrong bytes and moves a position the
    // guest thinks it owns.
    let at = if overlapped != 0 {
        let low = call.memory.read_u32(overlapped.wrapping_add(8))?;
        let high = call.memory.read_u32(overlapped.wrapping_add(12))?;
        if high != 0 {
            host.last_error = 87; // ERROR_INVALID_PARAMETER
            return Ok(Return(0));
        }
        Some(low)
    } else {
        None
    };

    if std::env::var("SHIMKIT_LOG_FILES").is_ok() && (overlapped != 0 || requested > 1024) {
        host.notice(format!(
            "read {handle:#x} {requested} bytes (overlapped {overlapped:#x}, offset {at:?})"
        ));
    }
    let Some((count, position)) = transfer(host, call, handle, destination, requested, at)? else {
        return Ok(Return(0));
    };
    // A short read leaves the rest of the guest's buffer as it was, and a
    // streamer that trusts its own size table walks into that stale data: the
    // count is worth a line of its own rather than being inferred.
    if std::env::var("SHIMKIT_LOG_READS").is_ok() && host.notices.len() < 900 {
        host.notice(format!(
            "read {handle:#x} gave {count}/{requested} at {position}"
        ));
    }
    if copied_out != 0 {
        call.write_u32(copied_out, count)?;
    }
    if overlapped != 0 {
        // The bytes are in the buffer and the completion is written where
        // `GetOverlappedResult` and an event wait look for it, but the call still
        // answers ERROR_IO_PENDING: a guest that streams files posts these reads
        // and runs the completion from whoever waits for the event, never from
        // the calling frame. Reporting success here makes it run that completion
        // inside the frame that holds the stream's lock, which deadlocks the
        // dispatcher against its own semaphore.
        call.write_u32(overlapped, 0)?; // Internal: STATUS_SUCCESS
        call.write_u32(overlapped.wrapping_add(4), count)?; // InternalHigh: transferred
        let event = call.memory.read_u32(overlapped.wrapping_add(16))?;
        if event != 0 {
            host.objects.signal(event);
            host.woken.extend(host.objects.wake(event));
        }
        host.last_error = ERROR_IO_PENDING;
        return Ok(Return(0));
    } else {
        host.files.seek(handle, position.saturating_add(count));
    }
    host.files.bytes_read += u64::from(count);
    Ok(Return(1))
}

/// The bytes one read transfers, and the file position it read from.
///
/// A read at or past the end of the file transfers nothing, which a guest that
/// seeked past the end (or posted an overlapped read there) is allowed to ask
/// for: the position is clamped so the slice stays inside the bytes. Short
/// reads are legal: the guest reads in chunks and stops at zero. A synthetic
/// entry reads as zeroes, exactly as `ReadFile` answers it, and the first read
/// from one is worth saying out loud: the guest was supposed to measure this
/// file, not read it. `None` is an invalid handle.
fn transfer(
    host: &mut Host,
    call: &mut Call<'_>,
    handle: u32,
    destination: u32,
    requested: u32,
    at: Option<u32>,
) -> Result<Option<(u32, u32)>> {
    let Some((entry, position)) = host.files.file(handle) else {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(None);
    };
    let position = at.unwrap_or(position).min(entry.length);
    let available = entry.length.saturating_sub(position);
    let count = requested.min(available);
    if entry.bytes.is_empty() && count > 0 {
        if host.files.synthetic_reads == 0 {
            host.notice("read from a synthetic CD marker: the guest wanted its contents".to_string());
        }
        host.files.synthetic_reads += 1;
        call.zero(destination, count)?;
    } else {
        let chunk = entry.bytes[position as usize..position as usize + count as usize].to_vec();
        call.write(destination, &chunk)?;
    }
    Ok(Some((count, position)))
}

/// `ReadFileEx(handle, buffer, count, overlapped, completion)`.
///
/// The read itself is the one `ReadFile` performs overlapped: the bytes land in
/// the buffer now, the `OVERLAPPED` records them where `GetOverlappedResult`
/// and an event wait look, and the event fires. What is different is the
/// completion routine: it is queued for the thread that posted the read, and
/// the session runs it as guest code on that thread at its next switch point,
/// the way a timer callback runs. The call answers success because the read is
/// queued rather than done -- in this host the bytes are already in memory, so
/// the queue only decides when the guest is told.
fn read_file_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let destination = call.arg(1)?;
    let requested = call.arg(2)?;
    let overlapped = call.arg(3)?;
    let completion = call.arg(4)?;
    if overlapped == 0 || completion == 0 {
        // There is no non-overlapped form, and no routine to run would leave a
        // completion that can never report itself.
        host.last_error = 87; // ERROR_INVALID_PARAMETER
        return Ok(Return(0));
    }
    let low = call.memory.read_u32(overlapped.wrapping_add(8))?;
    let high = call.memory.read_u32(overlapped.wrapping_add(12))?;
    if high != 0 {
        host.last_error = 87; // ERROR_INVALID_PARAMETER
        return Ok(Return(0));
    }
    let Some((count, _)) = transfer(host, call, handle, destination, requested, Some(low))?
    else {
        return Ok(Return(0));
    };
    if std::env::var("SHIMKIT_LOG_FILES").is_ok() && host.notices.len() < 600 {
        host.notice(format!(
            "read {handle:#x} {requested} bytes overlapped {overlapped:#x}, completion {completion:#010x}"
        ));
    }
    call.write_u32(overlapped, 0)?; // Internal: STATUS_SUCCESS
    call.write_u32(overlapped.wrapping_add(4), count)?; // InternalHigh: transferred
    let event = call.memory.read_u32(overlapped.wrapping_add(16))?;
    if event != 0 {
        host.objects.signal(event);
        host.woken.extend(host.objects.wake(event));
    }
    host.files.bytes_read += u64::from(count);
    // The cursor does not move: an overlapped read owns its own offset, and the
    // guest is free to seek or to post the next buffer while the completion
    // waits to run.
    host.file_apcs.push(FileApc {
        thread: host.current_thread,
        entry: completion,
        error: 0,
        transferred: count,
        overlapped,
    });
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
    use crate::objects::Object;

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
    fn a_game_file_is_written_to_a_copy_and_the_payload_stays_as_delivered() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // What a game does on start: open its own settings file and rewrite it.
        host.files.provide("data\\settings.dat", vec![1, 2, 3, 4]);
        let name = STACK_BASE + 0x9600;
        memory.write_bytes(name, b"data\\settings.dat\0").unwrap();
        let action = call_shim(
            &mut host,
            &mut memory,
            "CreateFileA",
            &[name, 0x4000_0000, 0, 0, CREATE_ALWAYS, 0x80, 0]
        )
        .unwrap();
        let Return(handle) = action else {
            panic!("CreateFileA returned {action:?}");
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "the folder is the guest's to write");
        assert_eq!(host.last_error, 0);

        // The write lands in the copy, and the guest reads it back.
        let buffer = STACK_BASE + 0x9600 + 0x200;
        let written = STACK_BASE + 0x9600 + 0x300;
        memory.write_bytes(buffer, b"new settings").unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "WriteFile",
                &[handle, buffer, 12, written, 0]
            )
            .unwrap(),
            Return(1)
        );
        assert_eq!(host.files.saved_files(), vec![("data\\settings.dat", &b"new settings"[..])]);
        // The bytes the page handed over are not the bytes the guest changed:
        // a reload reads the payload exactly as it was installed.
        assert_eq!(
            host.files.install.get("data\\settings.dat").unwrap().bytes,
            vec![1, 2, 3, 4]
        );

        // Opening it again is the same copy, not a second one.
        assert_ne!(
            call_shim(
                &mut host,
                &mut memory,
                "CreateFileA",
                &[name, 0x4000_0000, 0, 0, OPEN_EXISTING, 0x80, 0]
            )
            .unwrap(),
            Return(INVALID_HANDLE_VALUE)
        );
        assert_eq!(host.files.saved_files().len(), 1);
        assert!(host.notices.is_empty(), "writing a game file is not worth a notice");
    }

    #[test]
    fn a_copy_of_a_game_file_starts_as_the_bytes_provided() {
        let mut files = Files::default();
        files.provide("settings.dat", vec![7, 8, 9]);
        let handle = files
            .create("data\\settings.dat", OPEN_EXISTING)
            .expect("a provided file may be written");
        assert_eq!(files.length(handle), Some(3), "the copy starts as the file");
        assert!(files.saved("settings.dat"), "the page has something to persist");
        // `CREATE_NEW` still refuses a name that is there, copy or not.
        assert_eq!(files.create("settings.dat", CREATE_NEW), None);
        files.provide("layout.dat", vec![1]);
        assert_eq!(files.create("layout.dat", CREATE_NEW), None);
    }

    #[test]
    fn a_new_file_still_refuses_create_new_when_it_is_there() {
        let mut files = Files::default();
        assert!(files.create("save0001.dat", CREATE_ALWAYS).is_some());
        assert_eq!(files.create("save0001.dat", CREATE_NEW), None);
        assert_eq!(files.create("missing.dat", OPEN_EXISTING), None);
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
    fn a_deleted_file_is_gone_for_the_session_even_when_the_payload_supplied_it() {
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

        // The payload's files are the guest's to remove as well.
        let bundled = STACK_BASE + 0xa500;
        memory.write_bytes(bundled, b"sfx.dat\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteFileA", &[bundled]).unwrap(),
            Return(1)
        );
        assert!(!host.files.exists("sfx.dat"));
        assert!(host.files.find("sfx.dat").is_none(), "a listing does not show it");
        assert!(host.files.contents("sfx.dat").is_none());
        // Deleting it again is "no such file", which is what Windows answers.
        assert_eq!(
            call_shim(&mut host, &mut memory, "DeleteFileA", &[bundled]).unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_FILE_NOT_FOUND);
        // And the guest can make it again, empty.
        assert!(host.files.create("sfx.dat", CREATE_ALWAYS).is_some());
        assert_eq!(host.files.contents("sfx.dat"), Some(&[][..]));
    }

    #[test]
    fn a_read_only_handle_cannot_write_a_file_someone_else_opened_for_writing() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("settings.dat", vec![1, 2, 3]);
        let name = STACK_BASE + 0xa800;
        memory.write_bytes(name, b"settings.dat\0").unwrap();

        // A reader first, then the writer: the file is the guest's from the
        // moment the second open happens, but the first handle still cannot
        // write, because the right belongs to the handle and not to the file.
        let reader = open_sfx(&mut host, &mut memory);
        let writer = create_save(&mut host, &mut memory, b"settings.dat\0");
        let buffer = STACK_BASE + 0xa900;
        let written = STACK_BASE + 0xaa00;
        memory.write_bytes(buffer, b"changed").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "WriteFile", &[reader, buffer, 7, written, 0])
                .unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_ACCESS_DENIED);
        assert_eq!(host.files.contents("settings.dat"), Some(&[1, 2, 3][..]));

        assert_eq!(
            call_shim(&mut host, &mut memory, "WriteFile", &[writer, buffer, 7, written, 0])
                .unwrap(),
            Return(1)
        );
        assert_eq!(host.files.contents("settings.dat"), Some(&b"changed"[..]));
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

    /// A game that lists its own art asks for `.<dir>\*`, the current directory
    /// glued to a folder: the directory part is not part of the match, because
    /// the table is keyed by whole paths and every entry is "here".
    #[test]
    fn find_first_file_w_matches_a_directory_wildcard() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sprite/Loading/1.bmp", vec![1, 2]);
        let pattern = STACK_BASE + 0xa600;
        let data = STACK_BASE + 0xa700;
        for (index, unit) in ".\\sprite\\Loading\\*".encode_utf16().enumerate() {
            memory
                .write_u16(pattern + index as u32 * 2, unit)
                .unwrap();
        }
        memory.write_u16(pattern + 34, 0).unwrap();
        let action = call_shim(&mut host, &mut memory, "FindFirstFileW", &[pattern, data]).unwrap();
        let Return(search) = action else {
            panic!("FindFirstFileW returned {action:?}");
        };
        assert_ne!(search, INVALID_HANDLE_VALUE);
        let mut name = Vec::new();
        for index in 0..13 {
            name.push(memory.read_u16(data + 44 + index * 2).unwrap());
        }
        assert_eq!(String::from_utf16_lossy(&name), "1.bmp");
    }

    #[test]
    fn a_restored_save_is_writable_without_being_a_change() {
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
        // A payload file is not the guest's until the guest writes it.
        assert!(!files.saved("sfx.dat"));
        assert!(files.create("sfx.dat", CREATE_ALWAYS).is_some());
    }

    #[test]
    fn a_restored_save_shadows_the_payload_file_of_the_same_name() {
        let mut files = Files::default();
        // The page may hand either over first: a save shadows the install copy
        // instead of replacing it, so the order cannot lose one of them.
        files.provide_saved("settings.dat", vec![9, 9]);
        files.provide("settings.dat", vec![1, 2, 3]);
        assert_eq!(files.contents("settings.dat"), Some(&[9, 9][..]));
        assert!(files.saved("settings.dat"));

        let mut files = Files::default();
        files.provide("settings.dat", vec![1, 2, 3]);
        files.provide_saved("settings.dat", vec![9, 9]);
        assert_eq!(files.contents("settings.dat"), Some(&[9, 9][..]));

        // A save the payload does not ship is just a file.
        let mut files = Files::default();
        files.provide_saved("save0001.dat", vec![4]);
        assert_eq!(files.contents("save0001.dat"), Some(&[4][..]));
        assert!(files.install.is_empty());
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

    /// An `OVERLAPPED` the way the guest's streaming loader builds one: internal
    /// results, the offset, and the event that will be signalled.
    fn write_overlapped(
        memory: &mut GuestMemory,
        overlapped: u32,
        offset: u32,
        event: u32,
    ) -> Result<()> {
        memory.write_u32(overlapped, 0)?; // Internal
        memory.write_u32(overlapped.wrapping_add(4), 0)?; // InternalHigh
        memory.write_u32(overlapped.wrapping_add(8), offset)?; // Offset
        memory.write_u32(overlapped.wrapping_add(12), 0)?; // OffsetHigh
        memory.write_u32(overlapped.wrapping_add(16), event)?; // hEvent
        Ok(())
    }

    #[test]
    fn a_read_file_ex_fills_the_buffer_records_the_result_and_queues_the_completion() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![10, 20, 30, 40, 50, 60]);
        let handle = open_sfx(&mut host, &mut memory);
        let buffer = STACK_BASE + 0xb000;
        let overlapped = STACK_BASE + 0xb100;
        let completion = STACK_BASE + 0xb200;
        write_overlapped(&mut memory, overlapped, 2, 0).unwrap();
        let before = host.files.bytes_read();

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFileEx",
                &[handle, buffer, 4, overlapped, completion]
            )
            .unwrap(),
            Return(1)
        );
        // The bytes are where the guest will look for them, from the offset the
        // `OVERLAPPED` carries, and the cursor never moved.
        assert_eq!(memory.read_bytes(buffer, 4).unwrap(), &[30, 40, 50, 60]);
        assert_eq!(host.files.position(handle), Some(0));
        assert_eq!(memory.read_u32(overlapped).unwrap(), 0, "Internal");
        assert_eq!(
            memory.read_u32(overlapped.wrapping_add(4)).unwrap(),
            4,
            "InternalHigh: transferred"
        );
        assert_eq!(host.files.bytes_read(), before + 4);
        // The completion is queued for the thread that posted the read, with the
        // arguments the routine will be entered with.
        assert_eq!(host.file_apcs.len(), 1);
        let apc = host.file_apcs[0];
        assert_eq!(apc.thread, 0);
        assert_eq!(apc.entry, completion);
        assert_eq!(apc.error, 0);
        assert_eq!(apc.transferred, 4);
        assert_eq!(apc.overlapped, overlapped);
    }

    #[test]
    fn a_read_file_ex_at_the_end_of_the_file_completes_with_zero_bytes() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1, 2]);
        let handle = open_sfx(&mut host, &mut memory);
        let buffer = STACK_BASE + 0xb300;
        let overlapped = STACK_BASE + 0xb400;
        let completion = STACK_BASE + 0xb500;
        write_overlapped(&mut memory, overlapped, 2, 0).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFileEx",
                &[handle, buffer, 8, overlapped, completion]
            )
            .unwrap(),
            Return(1)
        );
        // A read at the end transfers nothing, and Windows still runs the
        // completion for it: zero bytes is an answer, not a failure.
        assert_eq!(host.file_apcs.len(), 1);
        assert_eq!(host.file_apcs[0].transferred, 0);
        assert_eq!(host.file_apcs[0].error, 0);
    }

    #[test]
    fn a_read_file_ex_on_a_bad_handle_fails_without_queueing_anything() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let buffer = STACK_BASE + 0xb600;
        let overlapped = STACK_BASE + 0xb700;
        let completion = STACK_BASE + 0xb800;
        write_overlapped(&mut memory, overlapped, 0, 0).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFileEx",
                &[0x1234, buffer, 4, overlapped, completion]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_INVALID_HANDLE);
        assert!(host.file_apcs.is_empty());
    }

    #[test]
    fn a_read_file_ex_without_a_structure_or_a_routine_is_rejected() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![1]);
        let handle = open_sfx(&mut host, &mut memory);
        let buffer = STACK_BASE + 0xb900;
        let completion = STACK_BASE + 0xba00;
        // No `OVERLAPPED`: there is no non-overlapped form of this call.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFileEx",
                &[handle, buffer, 1, 0, completion]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, 87);
        // No routine: the completion could never report itself.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFileEx",
                &[handle, buffer, 1, STACK_BASE + 0xbb00, 0]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, 87);
        assert!(host.file_apcs.is_empty());
    }

    #[test]
    fn a_read_file_ex_signals_the_event_the_overlapped_names() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.files.provide("sfx.dat", vec![7, 8, 9]);
        let handle = open_sfx(&mut host, &mut memory);
        let event = host.objects.insert(Object::Event {
            manual: true,
            signalled: false,
        });
        let buffer = STACK_BASE + 0xbc00;
        let overlapped = STACK_BASE + 0xbd00;
        let completion = STACK_BASE + 0xbe00;
        write_overlapped(&mut memory, overlapped, 0, event).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "ReadFileEx",
                &[handle, buffer, 3, overlapped, completion]
            )
            .unwrap(),
            Return(1)
        );
        assert!(
            host.objects.signalled(event),
            "a waiter on the event must not be left behind"
        );
        assert!(!host.woken.is_empty(), "the wake reaches the session");
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
