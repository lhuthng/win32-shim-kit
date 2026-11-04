//! The `ADVAPI32.dll` registry the boot needs: seven calls over one tree, and a
//! text form of that tree so a run's settings outlive the run.
//!
//! A guest keeps its installed settings in `HKLM\SOFTWARE\Vendor\Product`,
//! value `Data`, as `REG_BINARY`: on the first run it asks for the value's size,
//! gets `ERROR_FILE_NOT_FOUND`, writes a four-byte default, builds its defaults
//! and writes the real blob back. That is a path we can serve truthfully, but
//! only if the key exists -- and a rehost has no installer to create it. So
//! opening a missing key succeeds when the guest asks for write access and fails
//! with `ERROR_FILE_NOT_FOUND` when it asks read-only. That is the one deviation
//! from Windows in this file, and it stands in for the installer rather than for
//! the registry. `RegCreateKeyA` is how a guest that was told the key is missing
//! makes it, and `RegOpenKeyA` -- which has no access mask to read -- is always
//! the read-only open.
//!
//! # The store, and why it is not a hive
//!
//! A real registry hive is a binary format with allocation cells and a
//! transaction log, and reading one would buy this host nothing: no game reads
//! `SYSTEM.DAT` itself, and a guest sees only the values it asked for. What a
//! rehost *does* need is the three things a hive is for -- keep a setting across
//! runs, let the payload ship values as if an installer had written them, and
//! let a person inspect and edit them. So the tree has a text form
//! ([`Registry::to_text`] and [`Registry::from_text`]) that a payload can carry
//! as `registry.txt` and a diff can review:
//!
//! ```text
//! [HKEY_CURRENT_USER\Software\Hercules]
//! "Screen"=dword:00000280
//! "SavePath"="C:\\Hercules\\SAVE"
//! Blob=hex:03:0102ff
//! ```
//!
//! A section is a key, a `name=value` line is a value, and the type is the
//! prefix: `dword:`, a quoted string (`REG_SZ`), `expand:` (`REG_EXPAND_SZ`),
//! `multi:` (`REG_MULTI_SZ`), and `hex:TYPE:BYTES` for anything else, where
//! `TYPE` is the `REG_*` number in hex. The path in a section keeps the spelling
//! the text used, so a file someone wrote reads back the way they wrote it even
//! though the lookup is case-insensitive. Nothing here parses the guest's own
//! blob, so a value is bytes and a type, nothing more.
//!
//! # Finding what a title wants, and answering it
//!
//! Writing that file by hand needs the title's key paths, and a start-up rarely
//! mentions them in anything you can read: they are built at run time and passed
//! straight to `RegOpenKeyExA`. So every key an open names and every value a read
//! names is remembered -- whether it was there or not -- and
//! [`Registry::wanted_text`] gives back the ones that were *not* there, in the
//! text form, ready to fill in and drop into the payload:
//!
//! ```text
//! [HKEY_LOCAL_MACHINE\Software\Vendor\Product]
//! ;"Installed"=
//! ;"SavePath"=
//! ```
//!
//! A missing key becomes a real (empty) section, so uncommenting nothing already
//! makes the guest's read-only open succeed; each value it looked for beside it
//! is a commented line, because the type and content belong to the guest and a
//! plausible-looking value would be a guess presented as a setting.

use std::collections::{BTreeMap, BTreeSet, HashMap};

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

/// `RegCreateKeyExA`'s disposition: a key the call made, or one it found.
const REG_CREATED_NEW_KEY: u32 = 1;
const REG_OPENED_EXISTING_KEY: u32 = 2;

/// The `REG_*` types the text form names.
const REG_SZ: u32 = 1;
const REG_EXPAND_SZ: u32 = 2;
const REG_DWORD: u32 = 4;
const REG_MULTI_SZ: u32 = 7;

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
    /// The spelling each key was first written with, so the text form gives it
    /// back the way the guest (or the file) wrote it: a person reading
    /// `registry.txt` looks for `HKEY_CURRENT_USER`, not `hkey_current_user`.
    spelling: HashMap<String, String>,
    /// The same for value names, keyed by (path, lowercased name), so the text
    /// form writes `"Screen"` and not `"screen"`.
    names: HashMap<(String, String), String>,
    handles: HashMap<u32, String>,
    next_handle: u32,
    /// The keys an open named, and the values a read named within them, by the
    /// lowercased path the lookup uses -- kept whether or not they were found,
    /// because "what did this title ask for" is the other half of "what does it
    /// need".
    asked: BTreeMap<String, BTreeSet<String>>,
    /// The spelling each asked-for value name was passed with, so the report
    /// shows `"SavePath"` rather than `"savepath"`.
    asked_names: BTreeMap<(String, String), String>,
    /// Whether anything has been written since the last time the embedder asked,
    /// which is when the text form is worth regenerating.
    dirty: bool,
}

impl Registry {
    fn open(&mut self, path: String, create: bool) -> Option<u32> {
        self.asked.entry(path.clone()).or_default();
        if !self.keys.contains_key(&path) {
            if !create {
                return None;
            }
            self.keys.insert(path.clone(), HashMap::new());
            self.dirty = true;
        }
        let handle = FIRST_KEY + self.next_handle * 4;
        self.next_handle += 1;
        self.handles.insert(handle, path);
        Some(handle)
    }

    fn path_of(&self, handle: u32) -> Option<&String> {
        self.handles.get(&handle)
    }

    fn value(&self, path: &str, name: &str) -> Option<&Value> {
        self.keys.get(path).and_then(|key| key.get(name))
    }

    /// Remembers that the guest read `name` out of `path`, in the spelling it
    /// passed, so a value nothing wrote shows up in [`Self::wanted_text`].
    fn note_value(&mut self, path: &str, name: &str, written: &str) {
        self.asked.entry(path.to_owned()).or_default();
        let key = (path.to_owned(), name.to_owned());
        self.asked_names.entry(key).or_insert_with(|| written.to_owned());
        if let Some(values) = self.asked.get_mut(path) {
            values.insert(name.to_owned());
        }
    }

    /// Whether a key exists, without opening it: what `RegCreateKeyExA` needs to
    /// say `REG_CREATED_NEW_KEY` or `REG_OPENED_EXISTING_KEY`.
    pub fn has_path(&self, path: &str) -> bool {
        self.keys.contains_key(&path.to_lowercase())
    }

    /// The tree as text, for a payload to carry and a person to read.
    ///
    /// Keys are sorted and values are sorted within a key, so two runs that end
    /// in the same state produce the same bytes and a diff shows only what a
    /// player changed.
    pub fn to_text(&self) -> String {
        let mut keys: Vec<&String> = self.keys.keys().collect();
        keys.sort();
        let mut out = String::from(
            "; This host's registry, in its own text form. `[path]` opens a key, a\n\
             ; `name=value` line is one of its values, and the prefix says which type\n\
             ; (`dword:`, a quoted string, `expand:`, `multi:`, `hex:TYPE:BYTES`).\n",
        );
        for path in keys {
            let Some(values) = self.keys.get(path) else {
                continue;
            };
            out.push('\n');
            out.push('[');
            out.push_str(self.spelling.get(path).map_or(path.as_str(), String::as_str));
            out.push_str("]\n");
            let mut names: Vec<&String> = values.keys().collect();
            names.sort();
            for name in names {
                let Some(value) = values.get(name) else {
                    continue;
                };
                let written = self
                    .names
                    .get(&(path.clone(), name.clone()))
                    .map_or(name.as_str(), String::as_str);
                out.push_str(&quoted(written));
                out.push('=');
                out.push_str(&value_text(value));
                out.push('\n');
            }
        }
        out
    }

    /// The keys and values the guest read and did not find, written as the text
    /// form: the file a payload needs, with the blanks to fill in.
    ///
    /// A section is a key no one wrote; under it, and under any existing key with
    /// a value the guest looked for and did not get, a commented `"name"=` line
    /// says what it asked for. Uncommenting the section alone is already an
    /// answer -- an empty key opens -- so the report is useful before anything is
    /// typed into it.
    pub fn wanted_text(&self) -> String {
        let mut sections = String::new();
        for (path, asked) in &self.asked {
            let present = self.keys.contains_key(path);
            let wanted: Vec<&String> = asked
                .iter()
                .filter(|name| self.value(path, name).is_none())
                .collect();
            if present && wanted.is_empty() {
                continue;
            }
            sections.push('\n');
            sections.push('[');
            sections.push_str(&display_path(path));
            sections.push_str("]\n");
            for name in wanted {
                let written = self
                    .asked_names
                    .get(&(path.clone(), name.clone()))
                    .map_or(name.as_str(), String::as_str);
                sections.push(';');
                sections.push_str(&quoted(written));
                sections.push_str("=\n");
            }
        }
        if sections.is_empty() {
            return String::from("; Every key and value this run read was already there.\n");
        }
        format!("{WANTED_HEADER}{sections}")
    }

    /// Reads the text form, merging it over what is already there.
    ///
    /// The count is how many values were added, which is what a run reports when
    /// a payload shipped a prepared hive: zero says the file was empty or absent.
    pub fn from_text(&mut self, text: &str) -> Result<usize> {
        let mut current: Option<String> = None;
        let mut added = 0;
        for (index, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
                continue;
            }
            let number = index + 1;
            if let Some(rest) = line.strip_prefix('[') {
                let Some(path) = rest.strip_suffix(']') else {
                    return Err(format!("registry.txt line {number}: a section has no closing ]"));
                };
                let key = canonical_path(path);
                self.keys.entry(key.clone()).or_default();
                self.spelling.insert(key.clone(), path.trim().to_owned());
                current = Some(key);
                continue;
            }
            let Some(key) = current.as_ref() else {
                return Err(format!(
                    "registry.txt line {number}: a value before any [section]"
                ));
            };
            let Some((name, rest)) = split_value(line) else {
                return Err(format!("registry.txt line {number}: expected name=value"));
            };
            let value = parse_value(rest)
                .map_err(|why| format!("registry.txt line {number}: {why}"))?;
            self.put(key.clone(), &name, value);
            added += 1;
        }
        if added > 0 {
            self.dirty = true;
        }
        Ok(added)
    }

    /// Files one value, keeping the spelling its name was written with.
    fn put(&mut self, path: String, name: &str, value: Value) {
        let key = name.to_lowercase();
        self.names.insert((path.clone(), key.clone()), name.to_owned());
        self.keys.entry(path).or_default().insert(key, value);
    }

    /// Records a write, so an embedder knows the text form changed.
    pub fn note_write(&mut self) {
        self.dirty = true;
    }

    /// Whether anything changed since the last call, clearing the flag: what an
    /// embedder checks before it regenerates `registry.txt`.
    pub fn take_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.dirty, false)
    }

    /// The number of keys and values, so a run can report what it kept.
    pub fn size(&self) -> (usize, usize) {
        let values = self.keys.values().map(HashMap::len).sum::<usize>();
        (self.keys.len(), values)
    }
}

/// The key path a section header names, lowercased, with the hive's long name
/// folded to the short one the shims answer with.
///
/// A registry file is written `[HKEY_CURRENT_USER\Software\...]` because that is
/// what every description of the registry uses, while an open is joined onto the
/// short hive name -- so both spellings have to mean the same key, or a prepared
/// tree is present and unreachable at once.
fn canonical_path(path: &str) -> String {
    let lower = path.trim().to_lowercase();
    let (first, rest) = match lower.split_once('\\') {
        Some((first, rest)) => (first, Some(rest)),
        None => (lower.as_str(), None),
    };
    let hive = match first {
        "hkey_classes_root" | "hkcr" => "hkcr",
        "hkey_current_user" | "hkcu" => "hkcu",
        "hkey_local_machine" | "hklm" => "hklm",
        "hkey_users" | "hku" => "hku",
        "hkey_current_config" | "hkcc" => "hkcc",
        // Something this host has no hive for: leave it as written, so the
        // section still reads back the way it was typed.
        _ => return lower,
    };
    match rest {
        Some(rest) => format!("{hive}\\{rest}"),
        None => hive.to_string(),
    }
}

/// What [`Registry::wanted_text`] says above the sections it prints.
const WANTED_HEADER: &str = "; What this run read from the registry and did not find, as the text form.\n\
    ; A section is a key nothing had written: keep it and the guest's open finds a\n\
    ; key. A `;\"name\"=` line is a value it looked for beside the key: drop the `;`,\n\
    ; give the value, and the next run answers it. `;` starts a comment.\n";

/// A key path as a person writes it: the short hive name the shims join with, as
/// the long name every description of the registry uses.
fn display_path(path: &str) -> String {
    let (first, rest) = match path.split_once('\\') {
        Some((first, rest)) => (first, Some(rest)),
        None => (path, None),
    };
    let hive = match first {
        "hkcr" => "HKEY_CLASSES_ROOT",
        "hkcu" => "HKEY_CURRENT_USER",
        "hklm" => "HKEY_LOCAL_MACHINE",
        "hku" => "HKEY_USERS",
        "hkcc" => "HKEY_CURRENT_CONFIG",
        _ => return path.to_owned(),
    };
    match rest {
        Some(rest) => format!("{hive}\\{rest}"),
        None => hive.to_owned(),
    }
}

/// A name as the text form writes it: quoted, with `\\` and `"` escaped.
fn quoted(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        if character == '"' || character == '\\' {
            out.push('\\');
        }
        out.push(character);
    }
    out.push('"');
    out
}

/// A value as the text form writes it, by its `REG_*` type.
fn value_text(value: &Value) -> String {
    match value.kind {
        REG_DWORD if value.data.len() == 4 => {
            let bytes = [
                value.data[0],
                value.data[1],
                value.data[2],
                value.data[3],
            ];
            format!("dword:{:08x}", u32::from_le_bytes(bytes))
        }
        REG_SZ => quoted(&String::from_utf8_lossy(
            &value.data[..value.data.len().saturating_sub(1)],
        )),
        REG_EXPAND_SZ => format!(
            "expand:{}",
            quoted(&String::from_utf8_lossy(
                &value.data[..value.data.len().saturating_sub(1)]
            ))
        ),
        REG_MULTI_SZ => {
            // The stored form is NUL-separated, double-NUL-terminated; the text
            // form joins the strings with `" "` so a line stays one line.
            let text = String::from_utf8_lossy(&value.data);
            let parts: Vec<String> = text
                .split('\0')
                .filter(|part| !part.is_empty())
                .map(quoted)
                .collect();
            format!("multi:{}", parts.join(" "))
        }
        kind => {
            let hex: Vec<String> = value.data.iter().map(|byte| format!("{byte:02x}")).collect();
            format!("hex:{kind:x}:{}", hex.join(""))
        }
    }
}

/// Splits `name=value`, where the name may be quoted and the value may not.
fn split_value(line: &str) -> Option<(String, &str)> {
    if let Some(rest) = line.strip_prefix('"') {
        let mut name = String::new();
        let mut characters = rest.char_indices();
        while let Some((index, character)) = characters.next() {
            match character {
                '\\' => {
                    if let Some((_, escaped)) = characters.next() {
                        name.push(escaped);
                    }
                }
                '"' => return line[index + 2..].strip_prefix('=').map(|value| (name, value)),
                other => name.push(other),
            }
        }
        return None;
    }
    let (name, value) = line.split_once('=')?;
    Some((name.trim().to_owned(), value))
}

/// Reads one value in the text form.
fn parse_value(text: &str) -> std::result::Result<Value, String> {
    let text = text.trim();
    if let Some(number) = text.strip_prefix("dword:") {
        let parsed = u32::from_str_radix(number.trim(), 16)
            .map_err(|_| format!("`{number}` is not a hex dword"))?;
        return Ok(Value {
            kind: REG_DWORD,
            data: parsed.to_le_bytes().to_vec(),
        });
    }
    if text.starts_with('"') {
        return Ok(Value {
            kind: REG_SZ,
            data: with_terminator(unquote(text)?),
        });
    }
    if let Some(rest) = text.strip_prefix("expand:") {
        return Ok(Value {
            kind: REG_EXPAND_SZ,
            data: with_terminator(unquote(rest)?),
        });
    }
    if let Some(rest) = text.strip_prefix("multi:") {
        let mut data = Vec::new();
        for part in unquote_all(rest)? {
            data.extend_from_slice(part.as_bytes());
            data.push(0);
        }
        data.push(0);
        return Ok(Value {
            kind: REG_MULTI_SZ,
            data,
        });
    }
    if let Some(rest) = text.strip_prefix("hex:") {
        let (kind, bytes) = rest
            .split_once(':')
            .ok_or_else(|| "`hex:` needs `TYPE:BYTES`".to_string())?;
        let kind = u32::from_str_radix(kind.trim(), 16)
            .map_err(|_| format!("`{kind}` is not a hex REG_* type"))?;
        let digits: Vec<char> = bytes.chars().filter(|c| !c.is_whitespace()).collect();
        if digits.len() % 2 != 0 {
            return Err("an odd number of hex digits".to_string());
        }
        let mut data = Vec::with_capacity(digits.len() / 2);
        for pair in digits.chunks(2) {
            let text: String = pair.iter().collect();
            data.push(
                u8::from_str_radix(&text, 16)
                    .map_err(|_| format!("`{text}` is not a hex byte"))?,
            );
        }
        return Ok(Value { kind, data });
    }
    Err(format!("`{text}` does not start with a type"))
}

/// A string with the NUL the registry stores it with: a guest reading a
/// `REG_SZ` gets the terminator inside the length it was told.
fn with_terminator(text: String) -> Vec<u8> {
    let mut data = text.into_bytes();
    data.push(0);
    data
}

/// One quoted string, unescaped.
fn unquote(text: &str) -> std::result::Result<String, String> {
    let text = text.trim();
    let Some(rest) = text.strip_prefix('"') else {
        return Err("a string value must be quoted".to_string());
    };
    let mut out = String::new();
    let mut characters = rest.chars();
    while let Some(character) = characters.next() {
        match character {
            '\\' => {
                if let Some(escaped) = characters.next() {
                    out.push(escaped);
                }
            }
            '"' => return Ok(out),
            other => out.push(other),
        }
    }
    Err("a quoted string has no closing quote".to_string())
}

/// Several quoted strings, as `multi:` writes them.
fn unquote_all(text: &str) -> std::result::Result<Vec<String>, String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        out.push(unquote(rest)?);
        // Step past the closing quote of the one just read.
        let Some(end) = rest[1..].find('"') else {
            break;
        };
        rest = rest[end + 2..].trim_start();
    }
    Ok(out)
}

pub const SHIMS: &[Binding] = &[
    Binding { dll: ADVAPI32, name: "RegCloseKey", arg_bytes: 4, shim: reg_close_key },
    Binding { dll: ADVAPI32, name: "RegCreateKeyA", arg_bytes: 12, shim: reg_create_key },
    Binding { dll: ADVAPI32, name: "RegCreateKeyExA", arg_bytes: 36, shim: reg_create_key_ex },
    Binding { dll: ADVAPI32, name: "RegOpenKeyA", arg_bytes: 12, shim: reg_open_key },
    Binding { dll: ADVAPI32, name: "RegOpenKeyExA", arg_bytes: 20, shim: reg_open_key_ex },
    Binding { dll: ADVAPI32, name: "RegQueryValueExA", arg_bytes: 24, shim: reg_query_value_ex },
    Binding { dll: ADVAPI32, name: "RegSetValueExA", arg_bytes: 24, shim: reg_set_value_ex },
];

fn reg_open_key_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parent = call.arg(0)?;
    let subkey = optional_cstr(call, 1)?;
    let access = call.arg(3)?;
    let destination = call.arg(4)?;
    open(host, call, parent, subkey, access & KEY_WRITE != 0, destination)
}

/// `RegOpenKeyA(hkey, subkey, phkResult)`, which is `RegOpenKeyExA` with no
/// access mask: the guest asked to read, so a key the installer never wrote is
/// reported missing instead of being conjured.
fn reg_open_key(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parent = call.arg(0)?;
    let subkey = optional_cstr(call, 1)?;
    let destination = call.arg(2)?;
    open(host, call, parent, subkey, false, destination)
}

/// `RegCreateKeyExA(hkey, subkey, reserved, class, options, access, security,
/// phkResult, disposition)`: the same open, and it answers *whether it made the
/// key*.
///
/// That disposition is the part a guest uses: a first run writes its defaults
/// when it is told the key is new, and reads the player's settings when it is
/// told the key was already there. Answering `REG_CREATED_NEW_KEY` every time
/// would reset a player's settings on every boot, which is worse than failing.
fn reg_create_key_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parent = call.arg(0)?;
    let subkey = optional_cstr(call, 1)?;
    let destination = call.arg(7)?;
    let disposition = call.arg(8)?;
    if destination == 0 {
        return Ok(Return(ERROR_INVALID_HANDLE));
    }
    let Some(path) = join(host, parent, &subkey) else {
        return Ok(Return(ERROR_INVALID_HANDLE));
    };
    let existed = host.registry.has_path(&path);
    let Some(handle) = host.registry.open(path, true) else {
        return Ok(Return(ERROR_FILE_NOT_FOUND));
    };
    call.write_u32(destination, handle)?;
    if disposition != 0 {
        let answer = if existed {
            REG_OPENED_EXISTING_KEY
        } else {
            REG_CREATED_NEW_KEY
        };
        call.write_u32(disposition, answer)?;
    }
    Ok(Return(ERROR_SUCCESS))
}

/// `RegCreateKeyA(hkey, subkey, phkResult)`: opening that makes the key when it
/// is absent, which is what the guest's own first run does in place of an
/// installer.
fn reg_create_key(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let parent = call.arg(0)?;
    let subkey = optional_cstr(call, 1)?;
    let destination = call.arg(2)?;
    open(host, call, parent, subkey, true, destination)
}

/// The one path all three open calls take: resolve the parent, join the subkey,
/// and hand the guest a handle on the result.
fn open(
    host: &mut Host,
    call: &mut Call<'_>,
    parent: u32,
    subkey: String,
    create: bool,
    destination: u32,
) -> Result<Action> {
    if destination == 0 {
        return Ok(Return(ERROR_INVALID_HANDLE));
    }

    let Some(path) = join(host, parent, &subkey) else {
        return Ok(Return(ERROR_INVALID_HANDLE));
    };

    let opened = host.registry.open(path.clone(), create);
    log(
        host,
        format!(
            "registry: open {path} -> {}",
            match opened {
                Some(_) => "handle",
                None => "not found",
            }
        ),
    );
    match opened {
        Some(handle) => {
            call.write_u32(destination, handle)?;
            Ok(Return(ERROR_SUCCESS))
        }
        None => Ok(Return(ERROR_FILE_NOT_FOUND)),
    }
}

/// Says in the log what the guest asked the registry for, when asked to.
///
/// Off by default: a boot reads the registry a dozen times and every line is
/// ordinary. `SHIMKIT_LOG_REG` turns it on for a session that has to know the
/// order of the calls, and [`Registry::wanted_text`] answers the question people
/// actually ask -- what did it want that was not there -- without a log at all.
/// Bounded like the other query logs, so a loop cannot flood it.
fn log(host: &mut Host, line: String) {
    if std::env::var("SHIMKIT_LOG_REG").is_ok() && host.notices.len() < 600 {
        host.notice(line);
    }
}

/// The full path an open names: the parent -- a predefined hive, or a key this
/// host handed out, which is how an open on an open key is written -- joined
/// with the subkey, and lowercased because the registry does not distinguish
/// case.
fn join(host: &Host, parent: u32, subkey: &str) -> Option<String> {
    let base = match hive(parent) {
        Some(name) => name.to_string(),
        None => host.registry.path_of(parent)?.clone(),
    };
    let path = if subkey.is_empty() {
        base
    } else {
        format!("{}\\{}", base, subkey)
    };
    Some(path.to_lowercase())
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
    let written = optional_cstr(call, 1)?;
    let name = written.to_lowercase();
    let kind_out = call.arg(3)?;
    let data = call.arg(4)?;
    let size_out = call.arg(5)?;

    let Some(path) = host.registry.path_of(handle).cloned() else {
        return Ok(Return(ERROR_INVALID_HANDLE));
    };
    host.registry.note_value(&path, &name, &written);
    let Some(value) = host.registry.value(&path, &name).cloned() else {
        // The guest asked for a value the installer never wrote; that is a
        // first run, not an error in the host, and Windows answers it just as
        // quietly. It is only worth a line when someone is chasing the reads.
        if std::env::var("SHIMKIT_LOG_REGISTRY").is_ok() {
            log(
                host,
                format!("registry: {path} has no value \"{name}\" (asked for, not found)"),
            );
        }
        return Ok(Return(ERROR_FILE_NOT_FOUND));
    };
    if std::env::var("SHIMKIT_LOG_REGISTRY").is_ok() {
        log(
            host,
            format!("registry: {path} value \"{name}\" is {} byte(s)", value.data.len()),
        );
    }

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
    host.registry.put(path, &name, Value { kind, data: bytes });
    host.registry.note_write();
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
    fn reg_open_key_reports_a_missing_key_and_create_key_makes_it() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let subkey = STACK_BASE + 0x6c00;
        let destination = STACK_BASE + 0x6d00;
        memory
            .write_bytes(subkey, b"SOFTWARE\\Example\\Settings\0")
            .unwrap();

        // No installer ran, so the read-only open is told the key is absent.
        assert_eq!(
            call_shim(&mut host, &mut memory, "RegOpenKeyA", &[0x8000_0002, subkey, destination])
                .unwrap(),
            Return(ERROR_FILE_NOT_FOUND)
        );

        // The guest's first run creates it, and the same open then succeeds.
        assert_eq!(
            call_shim(&mut host, &mut memory, "RegCreateKeyA", &[0x8000_0002, subkey, destination])
                .unwrap(),
            Return(ERROR_SUCCESS)
        );
        assert!(host
            .registry
            .path_of(memory.read_u32(destination).unwrap())
            .unwrap()
            .ends_with("settings"));
        assert_eq!(
            call_shim(&mut host, &mut memory, "RegOpenKeyA", &[0x8000_0002, subkey, destination])
                .unwrap(),
            Return(ERROR_SUCCESS)
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

    #[test]
    fn a_prepared_tree_survives_the_text_form_unchanged() {
        let text = "[HKEY_CURRENT_USER\\Software\\Hercules]\n\
                    \"Screen\"=dword:00000280\n\
                    \"Path\"=\"C:\\\\SAVE\"\n";
        let mut registry = Registry::default();
        assert_eq!(registry.from_text(text).unwrap(), 2);
        assert_eq!(registry.size(), (1, 2));

        // What it writes is what it read: a payload that ships this file and a
        // run that ends in the same state produce the same bytes.
        let written = registry.to_text();
        assert!(written.contains("[HKEY_CURRENT_USER\\Software\\Hercules]"));
        assert!(written.contains("\"Screen\"=dword:00000280"));
        assert!(written.contains(r#""Path"="C:\\SAVE""#));

        // And the text reads back to the same tree it was written from.
        let mut second = Registry::default();
        assert_eq!(second.from_text(&written).unwrap(), 2);
        assert_eq!(second.size(), (1, 2));
    }

    #[test]
    fn a_section_named_with_the_long_hive_is_the_same_key_as_the_short_one() {
        let mut registry = Registry::default();
        assert_eq!(
            registry
                .from_text("[HKEY_CURRENT_USER\\Software\\Hercules]\n\"Screen\"=dword:00000280\n")
                .unwrap(),
            1
        );
        // The shims join an open onto the short name, so that is the key it must
        // have become.
        assert!(registry.has_path("hkcu\\software\\hercules"));
        // And the text form gives the file's own spelling back, because that is
        // what a person reading it looks for.
        assert!(registry
            .to_text()
            .contains("[HKEY_CURRENT_USER\\Software\\Hercules]"));
    }

    /// A title that reports itself uninstalled is a missing key, and the report
    /// has to be the file that fixes it: keeping the section as printed makes the
    /// very open that failed succeed, before any value is filled in.
    #[test]
    fn a_key_the_guest_found_missing_is_named_in_the_wanted_report() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let subkey = STACK_BASE + 0x7400;
        memory
            .write_bytes(subkey, b"SOFTWARE\\Disney Interactive\\Hercules\\1.00\0")
            .unwrap();
        let destination = STACK_BASE + 0x7500;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegOpenKeyA",
                &[0x8000_0002, subkey, destination]
            )
            .unwrap(),
            Return(ERROR_FILE_NOT_FOUND)
        );

        let report = host.registry.wanted_text();
        assert!(
            report.contains("[HKEY_LOCAL_MACHINE\\software\\disney interactive\\hercules\\1.00]"),
            "the missing key is named: {report}"
        );

        // The report is the answer, not just a description of the question.
        host.seed_registry(&report).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegOpenKeyA",
                &[0x8000_0002, subkey, destination]
            )
            .unwrap(),
            Return(ERROR_SUCCESS)
        );
    }

    /// A value the guest reads beside a key that does exist is a blank too, and it
    /// is a comment: the type is the guest's, and a guessed one would be a setting
    /// nobody wrote.
    #[test]
    fn a_value_the_guest_read_and_did_not_get_is_a_blank_in_the_report() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let handle = open_game_key(&mut host, &mut memory);
        let name = STACK_BASE + 0x7600;
        let size = STACK_BASE + 0x7700;
        memory.write_bytes(name, b"SavePath\0").unwrap();
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

        let report = host.registry.wanted_text();
        assert!(report.contains(";\"SavePath\"="), "the value is named: {report}");
        assert!(
            report.contains("[HKEY_LOCAL_MACHINE\\software\\example\\product]"),
            "beside the key it was read from: {report}"
        );
    }

    #[test]
    fn a_prepared_tree_answers_the_guests_first_read() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // What an installer would have left behind, shipped with the payload
        // instead of run again.
        assert_eq!(
            host.registry
                .from_text("[HKLM\\SOFTWARE\\Example\\Product]\n\"Installed\"=dword:00000001\n")
                .unwrap(),
            1
        );

        let subkey = STACK_BASE + 0x6e00;
        let destination = STACK_BASE + 0x6f00;
        memory
            .write_bytes(subkey, b"SOFTWARE\\Example\\Product\0")
            .unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegOpenKeyA",
                &[0x8000_0002, subkey, destination]
            )
            .unwrap(),
            Return(ERROR_SUCCESS)
        );

        let handle = memory.read_u32(destination).unwrap();
        let name = STACK_BASE + 0x7000;
        let data = STACK_BASE + 0x7100;
        let size = STACK_BASE + 0x7200;
        memory.write_bytes(name, b"Installed\0").unwrap();
        memory.write_u32(size, 4).unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "RegQueryValueExA",
                &[handle, name, 0, 0, data, size]
            )
            .unwrap(),
            Return(ERROR_SUCCESS)
        );
        assert_eq!(memory.read_u32(data).unwrap(), 1);
    }
}
