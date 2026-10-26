//! MSVC's C++ runtime: `std::basic_string<char>` and the file streams, as VC6
//! compiled them.
//!
//! A C++ program built with Visual C++ calls its standard library through the
//! import table, not through inline code: `std::string` and its operators are
//! ordinary imported functions, and the object they work on is a plain struct the
//! compiler allocates. So the whole of `std::string` is a surface this host can
//! answer, provided it agrees with the object layout the program was compiled
//! against.
//!
//! # The layout
//!
//! VC6's `basic_string<char>` is sixteen bytes: a pointer to the characters, the
//! length, and the capacity, at offsets 0, 4 and 8. The allocator follows as an
//! empty member. That was checked against a real binary rather than assumed: this
//! module's target lays two strings out sixteen bytes apart and passes
//! `substr`'s result buffer as the first stack argument. The distinction matters
//! because only *this* module reads those fields -- the program never does, and
//! every access goes through these entry points -- so the three-field layout is
//! what keeps the two sides agreeing.
//!
//! A capacity of zero means the characters live in one shared empty block; a
//! capacity above zero means the string owns a block of `capacity + 1` bytes from
//! the host arena, with the terminator at the end. That is the rule
//! `release` uses instead of tracking the allocation separately.
//!
//! # The streams
//!
//! `basic_ifstream`, `basic_ofstream`, `operator<<` and `getline` are modelled the
//! same way: by their behaviour and by the two ABI facts the game's own code
//! shows. First, the constructors of a class with virtual bases take a hidden
//! `int` -- the "this is the most-derived object" flag -- and every call site in
//! the target pushes it, so `basic_ifstream(path, mode)` arrives as twelve bytes
//! and `basic_ofstream()` as four. Second, `ios_base::eof()` is reached through
//! the object's first dword: MSVC6's iostreams inherit `basic_ios` virtually, and
//! the caller does `mov eax, [object]; mov edx, [eax + 4]; lea ecx, [object + edx]`
//! before the call. A stream object therefore has to hold a table pointer, or the
//! guest would follow whatever the stack happened to contain. See `table`.
//!
//! A stream's state -- its file, its `iostate` -- is kept here rather than inside
//! the guest object. Nothing in the object is read by the program: every method it
//! uses is an import, so the fields stay this host's business. That is also why
//! the object's size does not have to be known.
//!
//! A stream is in text mode unless its open mode says `ios::binary` (`0x20`), and
//! this target's log is opened with `out|trunc` (`0x12`). Text mode is the same
//! conversion a text-mode `FILE` does: a newline the program writes is in the file
//! as CRLF, and a CRLF in the file arrives as one newline. Getting that wrong is
//! invisible to a program that only reads back what it wrote, and visible the
//! moment its output is compared with a file a real Windows build produced.

use crate::memory::GuestMemory;
use crate::shim::crt::cstr_bytes;
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

/// The name this table is filed under.
///
/// The C++ runtime's object layout is version-specific: `msvcp71` and later moved
/// `basic_string` to a different representation, so a table that answered for the
/// whole family would corrupt every string a program holds. Only VC6's is
/// modelled.
pub const MSVCP60: &str = "MSVCP60.dll";

/// Offsets of the three fields this module reads and writes.
const POINTER: u32 = 0;
const LENGTH: u32 = 4;
const CAPACITY: u32 = 8;

/// `basic_string::npos`.
const NPOS: u32 = 0xffff_ffff;

// The mangled names are the imports themselves: `?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@`
// is `basic_string<char, char_traits<char>, allocator<char>>` and is spelled the
// same way in every name below.
pub const SHIMS: &[Binding] = &[
    // ---- construction and destruction --------------------------------
    Binding { dll: MSVCP60, name: "??0?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@ABV?$allocator@D@1@@Z", arg_bytes: 4, shim: construct_empty },
    Binding { dll: MSVCP60, name: "??0?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@PBDABV?$allocator@D@1@@Z", arg_bytes: 8, shim: construct_from_text },
    Binding { dll: MSVCP60, name: "??0?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@ABV01@@Z", arg_bytes: 4, shim: construct_from_string },
    Binding { dll: MSVCP60, name: "??_F?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEXXZ", arg_bytes: 0, shim: construct_empty },
    Binding { dll: MSVCP60, name: "??1?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAE@XZ", arg_bytes: 0, shim: destruct },
    Binding { dll: MSVCP60, name: "??4?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV01@ABV01@@Z", arg_bytes: 4, shim: assign_string },
    Binding { dll: MSVCP60, name: "??4?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV01@PBD@Z", arg_bytes: 4, shim: assign_text },
    // ---- accessors ---------------------------------------------------
    Binding { dll: MSVCP60, name: "?length@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBEIXZ", arg_bytes: 0, shim: length },
    Binding { dll: MSVCP60, name: "?empty@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBE_NXZ", arg_bytes: 0, shim: is_empty },
    Binding { dll: MSVCP60, name: "?c_str@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBEPBDXZ", arg_bytes: 0, shim: characters },
    Binding { dll: MSVCP60, name: "?begin@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEPADXZ", arg_bytes: 0, shim: characters },
    Binding { dll: MSVCP60, name: "??A?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAADI@Z", arg_bytes: 4, shim: index },
    Binding { dll: MSVCP60, name: "?end@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEPADXZ", arg_bytes: 0, shim: end },
    // ---- modifying ---------------------------------------------------
    Binding { dll: MSVCP60, name: "??Y?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV01@ABV01@@Z", arg_bytes: 4, shim: append_string },
    Binding { dll: MSVCP60, name: "??Y?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV01@D@Z", arg_bytes: 4, shim: append_character },
    Binding { dll: MSVCP60, name: "??Y?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV01@PBD@Z", arg_bytes: 4, shim: append_text },
    Binding { dll: MSVCP60, name: "?erase@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV12@II@Z", arg_bytes: 8, shim: erase_range },
    Binding { dll: MSVCP60, name: "?erase@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEPADPAD@Z", arg_bytes: 4, shim: erase_to_end },
    Binding { dll: MSVCP60, name: "?insert@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV12@IABV12@@Z", arg_bytes: 8, shim: insert_string },
    Binding { dll: MSVCP60, name: "?insert@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QAEAAV12@IPBD@Z", arg_bytes: 8, shim: insert_text },
    Binding { dll: MSVCP60, name: "?find@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBEIABV12@I@Z", arg_bytes: 8, shim: find },
    Binding { dll: MSVCP60, name: "?substr@?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@std@@QBE?AV12@II@Z", arg_bytes: 12, shim: substr },
    // ---- the free operators ------------------------------------------
    Binding { dll: MSVCP60, name: "??8std@@YA_NABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@0@Z", arg_bytes: 8, shim: equal },
    Binding { dll: MSVCP60, name: "??9std@@YA_NABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@0@Z", arg_bytes: 8, shim: not_equal },
    Binding { dll: MSVCP60, name: "??8std@@YA_NABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@PBD@Z", arg_bytes: 8, shim: equal_text },
    Binding { dll: MSVCP60, name: "??9std@@YA_NABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@PBD@Z", arg_bytes: 8, shim: not_equal_text },
    Binding { dll: MSVCP60, name: "??Mstd@@YA_NABV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@0@Z", arg_bytes: 8, shim: less },
    Binding { dll: MSVCP60, name: "??Hstd@@YA?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@ABV10@0@Z", arg_bytes: 12, shim: concat_strings },
    Binding { dll: MSVCP60, name: "??Hstd@@YA?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@ABV10@PBD@Z", arg_bytes: 12, shim: concat_string_text },
    Binding { dll: MSVCP60, name: "??Hstd@@YA?AV?$basic_string@DU?$char_traits@D@std@@V?$allocator@D@2@@0@PBDABV10@@Z", arg_bytes: 12, shim: concat_text_string },
    // ---- the file streams --------------------------------------------
    // The constructors' sizes are the declared parameters plus the hidden `int`
    // MSVC passes for a class with virtual bases; every call site in the target
    // pushes it. The destructions do not take it.
    Binding { dll: MSVCP60, name: "??0?$basic_ofstream@DU?$char_traits@D@std@@@std@@QAE@XZ", arg_bytes: 4, shim: ofstream_construct },
    Binding { dll: MSVCP60, name: "?open@?$basic_ofstream@DU?$char_traits@D@std@@@std@@QAEXPBDH@Z", arg_bytes: 8, shim: ofstream_open },
    Binding { dll: MSVCP60, name: "?close@?$basic_ofstream@DU?$char_traits@D@std@@@std@@QAEXXZ", arg_bytes: 0, shim: ofstream_close },
    Binding { dll: MSVCP60, name: "??_D?$basic_ofstream@DU?$char_traits@D@std@@@std@@QAEXXZ", arg_bytes: 0, shim: stream_destruct },
    Binding { dll: MSVCP60, name: "??0?$basic_ifstream@DU?$char_traits@D@std@@@std@@QAE@PBDH@Z", arg_bytes: 12, shim: ifstream_construct },
    Binding { dll: MSVCP60, name: "?is_open@?$basic_ifstream@DU?$char_traits@D@std@@@std@@QBE_NXZ", arg_bytes: 0, shim: is_open },
    Binding { dll: MSVCP60, name: "?getline@?$basic_istream@DU?$char_traits@D@std@@@std@@QAEAAV12@PADH@Z", arg_bytes: 8, shim: getline },
    Binding { dll: MSVCP60, name: "?eof@ios_base@std@@QBE_NXZ", arg_bytes: 0, shim: eof },
    Binding { dll: MSVCP60, name: "??_D?$basic_ifstream@DU?$char_traits@D@std@@@std@@QAEXXZ", arg_bytes: 0, shim: stream_destruct },
    Binding { dll: MSVCP60, name: "??6std@@YAAAV?$basic_ostream@DU?$char_traits@D@std@@@0@AAV10@PBD@Z", arg_bytes: 8, shim: write_text },
    Binding { dll: MSVCP60, name: "??6?$basic_ostream@DU?$char_traits@D@std@@@std@@QAEAAV01@P6AAAV01@AAV01@@Z@Z", arg_bytes: 4, shim: write_manipulator },
    Binding { dll: MSVCP60, name: "?endl@std@@YAAAV?$basic_ostream@DU?$char_traits@D@std@@@1@AAV21@@Z", arg_bytes: 4, shim: endl },
    // ---- the one piece of unrelated runtime an initialiser touches -----
    Binding { dll: MSVCP60, name: "??0_Lockit@std@@QAE@XZ", arg_bytes: 0, shim: lockit },
    Binding { dll: MSVCP60, name: "??1_Lockit@std@@QAE@XZ", arg_bytes: 0, shim: lockit },
];

/// State the C++ runtime keeps for the session.
#[derive(Default)]
pub struct Strings {
    /// The one-byte block a string with no capacity points at, allocated on first
    /// use: `c_str()` of an empty string must be a readable `""`.
    empty: Option<u32>,
    /// Where `basic_string::npos` lives, for the data import that reads it.
    npos: Option<u32>,
    /// The virtual-base table pointer every constructed stream object carries.
    table_at: Option<u32>,
    /// The streams this session has constructed, by the address of the object.
    streams: std::collections::BTreeMap<u32, Stream>,
}

// ---- the object ---------------------------------------------------------

/// A string's characters and length.
fn fields(memory: &GuestMemory, string: u32) -> Result<(u32, u32)> {
    Ok((
        memory.read_u32(string + POINTER)?,
        memory.read_u32(string + LENGTH)?,
    ))
}

/// A string's contents, by length rather than to a terminator: a `std::string` may
/// hold a NUL.
fn content(memory: &GuestMemory, string: u32) -> Result<Vec<u8>> {
    let (at, length) = fields(memory, string)?;
    if length == 0 {
        return Ok(Vec::new());
    }
    if at == 0 {
        return Err(format!(
            "string at {string:#010x} has length {length} but no characters"
        ));
    }
    Ok(memory.read_bytes(at, length as usize)?.to_vec())
}

/// The shared pointer an empty string carries.
fn empty_pointer(host: &mut Host) -> Result<u32> {
    if let Some(at) = host.msvcp.empty {
        return Ok(at);
    }
    // The arena is zeroed where it has never been used, and a one-byte block that
    // is never written reads as `""` either way.
    let at = host.allocate(1, 4)?;
    host.msvcp.empty = Some(at);
    Ok(at)
}

/// Writes the empty representation into a fresh object.
fn construct(host: &mut Host, memory: &mut GuestMemory, string: u32) -> Result<()> {
    let empty = empty_pointer(host)?;
    memory.write_u32(string + POINTER, empty)?;
    memory.write_u32(string + LENGTH, 0)?;
    memory.write_u32(string + CAPACITY, 0)
}

/// Gives the string's own block back, if it has one.
fn release(host: &mut Host, memory: &mut GuestMemory, string: u32) -> Result<()> {
    let capacity = memory.read_u32(string + CAPACITY)?;
    if capacity > 0 {
        host.release(memory.read_u32(string + POINTER)?);
    }
    Ok(())
}

/// Makes sure the string can hold `needed` characters and its terminator.
///
/// The block grows by doubling, so appending in a loop stays linear, and the
/// terminator slot is written when the block is made: every later write appends
/// before it.
fn reserve(
    host: &mut Host,
    memory: &mut GuestMemory,
    string: u32,
    needed: u32,
) -> Result<()> {
    let capacity = memory.read_u32(string + CAPACITY)?;
    if needed <= capacity {
        return Ok(());
    }
    let grown = needed.saturating_mul(2).max(needed.saturating_add(8));
    let block = host.allocate(grown + 1, 4)?;
    memory.write_u8(block + grown, 0)?;
    let (at, length) = fields(memory, string)?;
    if length > 0 {
        memory.copy(at, block, length as usize)?;
    }
    if capacity > 0 {
        host.release(at);
    }
    memory.write_u32(string + POINTER, block)?;
    memory.write_u32(string + CAPACITY, grown)
}

/// Replaces the contents with `bytes`.
fn assign(host: &mut Host, memory: &mut GuestMemory, string: u32, bytes: &[u8]) -> Result<()> {
    let length = bytes.len() as u32;
    reserve(host, memory, string, length)?;
    let at = memory.read_u32(string + POINTER)?;
    memory.write_bytes(at, bytes)?;
    memory.write_u8(at + length, 0)?;
    memory.write_u32(string + LENGTH, length)
}

/// Appends `bytes` to the end.
fn append(host: &mut Host, memory: &mut GuestMemory, string: u32, bytes: &[u8]) -> Result<()> {
    let (_, length) = fields(memory, string)?;
    let total = length
        .checked_add(bytes.len() as u32)
        .ok_or_else(|| "string grows past the address space".to_string())?;
    reserve(host, memory, string, total)?;
    let at = memory.read_u32(string + POINTER)?;
    memory.write_bytes(at + length, bytes)?;
    memory.write_u8(at + total, 0)?;
    memory.write_u32(string + LENGTH, total)
}

/// Inserts `bytes` so that they start at `position`, clamping a position past the
/// end to the end.
fn insert(
    host: &mut Host,
    memory: &mut GuestMemory,
    string: u32,
    position: u32,
    bytes: &[u8],
) -> Result<()> {
    let existing = content(memory, string)?;
    let position = (position as usize).min(existing.len());
    let total = existing.len() + bytes.len();
    let mut merged = Vec::with_capacity(total);
    merged.extend_from_slice(&existing[..position]);
    merged.extend_from_slice(bytes);
    merged.extend_from_slice(&existing[position..]);
    assign(host, memory, string, &merged)
}

// ---- construction and destruction ---------------------------------------

/// `basic_string(const allocator&)` and the default-constructor closure: an empty
/// string. The allocator is the caller's, and this object keeps none.
fn construct_empty(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    construct(host, call.memory, string)?;
    Ok(Return(string))
}

/// `basic_string(const char*, const allocator&)`.
fn construct_from_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let bytes = cstr_bytes(call.memory, call.arg(0)?)?;
    construct(host, call.memory, string)?;
    assign(host, call.memory, string, &bytes)?;
    Ok(Return(string))
}

/// `basic_string(const basic_string&)`.
fn construct_from_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let bytes = content(call.memory, call.arg(0)?)?;
    construct(host, call.memory, string)?;
    assign(host, call.memory, string, &bytes)?;
    Ok(Return(string))
}

/// `~basic_string()`, and the deleting-destructor closure the runtime exports the
/// same way.
fn destruct(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    release(host, call.memory, string)?;
    construct(host, call.memory, string)?;
    Ok(Return(0))
}

/// `operator=(const basic_string&)`.
fn assign_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    // A self-assignment would read the block it is about to overwrite, so the
    // source is copied first.
    let bytes = content(call.memory, call.arg(0)?)?;
    assign(host, call.memory, string, &bytes)?;
    Ok(Return(string))
}

/// `operator=(const char*)`.
fn assign_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let bytes = cstr_bytes(call.memory, call.arg(0)?)?;
    assign(host, call.memory, string, &bytes)?;
    Ok(Return(string))
}

// ---- accessors ----------------------------------------------------------

/// `length()`.
fn length(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.memory.read_u32(host.this + LENGTH)?))
}

/// `empty()`.
fn is_empty(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from(
        call.memory.read_u32(host.this + LENGTH)? == 0,
    )))
}

/// `c_str()` and `begin()`: the first character, which for an empty string is the
/// shared terminator.
fn characters(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.memory.read_u32(host.this + POINTER)?))
}

/// `operator[](position)`: the address of the character, which the caller reads.
fn index(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let position = call.arg(0)?;
    let at = call.memory.read_u32(host.this + POINTER)?;
    Ok(Return(at.wrapping_add(position)))
}

/// `end()`: one past the last character.
fn end(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (at, length) = fields(call.memory, host.this)?;
    Ok(Return(at.wrapping_add(length)))
}

// ---- modifying ----------------------------------------------------------

/// `operator+=(const basic_string&)`.
fn append_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let bytes = content(call.memory, call.arg(0)?)?;
    append(host, call.memory, string, &bytes)?;
    Ok(Return(string))
}

/// `operator+=(char)`.
fn append_character(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let character = call.arg(0)? as u8;
    append(host, call.memory, string, &[character])?;
    Ok(Return(string))
}

/// `operator+=(const char*)`.
fn append_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let bytes = cstr_bytes(call.memory, call.arg(0)?)?;
    append(host, call.memory, string, &bytes)?;
    Ok(Return(string))
}

/// `erase(position, count)`.
fn erase_range(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let position = call.arg(0)?;
    let count = call.arg(1)?;
    let existing = content(call.memory, string)?;
    let start = (position as usize).min(existing.len());
    let removed = (count as usize).min(existing.len() - start);
    let mut kept = Vec::with_capacity(existing.len() - removed);
    kept.extend_from_slice(&existing[..start]);
    kept.extend_from_slice(&existing[start + removed..]);
    assign(host, call.memory, string, &kept)?;
    Ok(Return(string))
}

/// `erase(where)`: everything from that character to the end, answering the new
/// end.
fn erase_to_end(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let where_ = call.arg(0)?;
    let (at, length) = fields(call.memory, string)?;
    let position = where_.saturating_sub(at).min(length);
    let kept = content(call.memory, string)?[..position as usize].to_vec();
    assign(host, call.memory, string, &kept)?;
    Ok(Return(at.wrapping_add(position)))
}

/// `insert(position, const basic_string&)`.
fn insert_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let position = call.arg(0)?;
    let bytes = content(call.memory, call.arg(1)?)?;
    insert(host, call.memory, string, position, &bytes)?;
    Ok(Return(string))
}

/// `insert(position, const char*)`.
fn insert_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let string = host.this;
    let position = call.arg(0)?;
    let bytes = cstr_bytes(call.memory, call.arg(1)?)?;
    insert(host, call.memory, string, position, &bytes)?;
    Ok(Return(string))
}

/// `find(const basic_string&, position)`: the index of the first occurrence at or
/// after `position`, or `npos`.
fn find(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let needle = content(call.memory, call.arg(0)?)?;
    let position = call.arg(1)?;
    let haystack = content(call.memory, host.this)?;
    if position as usize > haystack.len() {
        return Ok(Return(NPOS));
    }
    if needle.is_empty() {
        return Ok(Return(position));
    }
    let found = haystack[position as usize..]
        .windows(needle.len())
        .position(|window| window == needle.as_slice());
    Ok(Return(match found {
        Some(index) => position + index as u32,
        None => NPOS,
    }))
}

/// `substr(position, count)`: the first stack argument is where the result goes,
/// which is how a `__cdecl` function returns a class by value.
fn substr(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let result = call.arg(0)?;
    let position = call.arg(1)?;
    let count = call.arg(2)?;
    let existing = content(call.memory, host.this)?;
    let start = (position as usize).min(existing.len());
    let length = (count as usize).min(existing.len() - start);
    let slice = existing[start..start + length].to_vec();
    construct(host, call.memory, result)?;
    assign(host, call.memory, result, &slice)?;
    Ok(Return(result))
}

// ---- the free operators -------------------------------------------------

/// The bytes of both operands of a `(const string&, const string&)` operator.
fn both(memory: &GuestMemory, call: &Call<'_>) -> Result<(Vec<u8>, Vec<u8>)> {
    Ok((
        content(memory, call.arg(0)?)?,
        content(memory, call.arg(1)?)?,
    ))
}

/// The string and the C string of a `(const string&, const char*)` operator.
fn string_and_text(memory: &GuestMemory, call: &Call<'_>) -> Result<(Vec<u8>, Vec<u8>)> {
    Ok((
        content(memory, call.arg(0)?)?,
        cstr_bytes(memory, call.arg(1)?)?,
    ))
}

/// `operator==(const string&, const string&)`.
fn equal(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (left, right) = both(call.memory, call)?;
    Ok(Return(u32::from(left == right)))
}

/// `operator!=(const string&, const string&)`.
fn not_equal(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (left, right) = both(call.memory, call)?;
    Ok(Return(u32::from(left != right)))
}

/// `operator==(const string&, const char*)`.
fn equal_text(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (left, right) = string_and_text(call.memory, call)?;
    Ok(Return(u32::from(left == right)))
}

/// `operator!=(const string&, const char*)`.
fn not_equal_text(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (left, right) = string_and_text(call.memory, call)?;
    Ok(Return(u32::from(left != right)))
}

/// `operator<(const string&, const string&)`: a byte comparison, which is what
/// `char_traits<char>::compare` is.
fn less(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (left, right) = both(call.memory, call)?;
    Ok(Return(u32::from(left < right)))
}

/// `operator+(const string&, const string&)`.
///
/// The result buffer is the first stack argument, so the two operands start at
/// index 1 -- unlike the comparison operators, which have no result buffer and
/// start at 0.
fn concat_strings(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let result = call.arg(0)?;
    let mut bytes = content(call.memory, call.arg(1)?)?;
    bytes.extend_from_slice(&content(call.memory, call.arg(2)?)?);
    construct(host, call.memory, result)?;
    assign(host, call.memory, result, &bytes)?;
    Ok(Return(result))
}

/// `operator+(const string&, const char*)`.
fn concat_string_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let result = call.arg(0)?;
    let mut bytes = content(call.memory, call.arg(1)?)?;
    bytes.extend_from_slice(&cstr_bytes(call.memory, call.arg(2)?)?);
    construct(host, call.memory, result)?;
    assign(host, call.memory, result, &bytes)?;
    Ok(Return(result))
}

/// `operator+(const char*, const string&)`.
fn concat_text_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let result = call.arg(0)?;
    let mut bytes = cstr_bytes(call.memory, call.arg(1)?)?;
    bytes.extend_from_slice(&content(call.memory, call.arg(2)?)?);
    construct(host, call.memory, result)?;
    assign(host, call.memory, result, &bytes)?;
    Ok(Return(result))
}

// ---- the streams --------------------------------------------------------

/// `ios_base::iostate` bits, as VC6 numbers them.
const BADBIT: u32 = 0x1;
const EOFLBIT: u32 = 0x2;

/// `ios::binary`. Without it a stream is in text mode, where the runtime converts
/// between the file's CRLF and the program's newline -- the same conversion the
/// C runtime's text-mode `FILE` does. The target's `open` call passes
/// `out|trunc` (`0x12`), which is text.
const BINARY: u32 = 0x20;

/// One open stream: the file behind it, which way it points, and its state.
struct Stream {
    /// The handle once the file is attached. An output stream with no successful
    /// `open` has none, and so does an input stream whose file is absent.
    handle: Option<u32>,
    /// Whether `operator<<` may write to it. The object address does not say:
    /// `open` and `close` are the same imports for both classes.
    writing: bool,
    /// Whether the file's bytes are the program's bytes.
    binary: bool,
    /// The `iostate` bits this host has raised.
    state: u32,
}

/// The virtual-base table every stream object points at.
///
/// MSVC6's iostreams inherit `basic_ios` virtually, so a call on the virtual base
/// -- `ios_base::eof()` is the one this target makes -- first loads the object's
/// first dword and reads the offset from the table it finds there:
/// `mov eax, [object]; mov edx, [eax + 4]; lea ecx, [object + edx]`. Constructing
/// a stream therefore has to leave a readable table behind its pointer, or the
/// guest follows whatever was on the stack.
///
/// This host models the virtual base at offset zero. Nothing virtual is ever
/// dispatched through the table -- every method the guest calls is an import, and
/// the virtual base's own methods are imports too -- so one all-zero table answers
/// for the object and all of its bases, and the pointer in the object identifies
/// it as a stream this host constructed.
fn table(host: &mut Host, call: &mut Call<'_>) -> Result<u32> {
    if let Some(at) = host.msvcp.table_at {
        return Ok(at);
    }
    let at = host.allocate(8, 4)?;
    call.zero(at, 8)?;
    host.msvcp.table_at = Some(at);
    Ok(at)
}

/// Registers the object `this` points at as a stream and writes its table pointer.
fn attach(
    host: &mut Host,
    call: &mut Call<'_>,
    writing: bool,
    binary: bool,
    handle: Option<u32>,
    state: u32,
) -> Result<()> {
    let object = host.this;
    let at = table(host, call)?;
    call.write_u32(object, at)?;
    host.msvcp.streams.insert(
        object,
        Stream {
            handle,
            writing,
            binary,
            state,
        },
    );
    Ok(())
}

/// The stream `this` names, or the error that says it is not one.
fn stream_of<'a>(host: &'a mut Host) -> Result<(&'a mut Stream, u32)> {
    let object = host.this;
    match host.msvcp.streams.get_mut(&object) {
        Some(stream) => Ok((stream, object)),
        None => Err(format!(
            "stream at {object:#010x} was never constructed by this host"
        )),
    }
}

/// Raises `bits` on the stream `this` names.
fn raise(host: &mut Host, bits: u32) {
    if let Some(stream) = host.msvcp.streams.get_mut(&host.this) {
        stream.state |= bits;
    }
}

/// The path an `open` was given: a C string the caller may have built on the
/// stack out of two `std::string`s.
fn path_of(call: &Call<'_>) -> Result<String> {
    let bytes = cstr_bytes(call.memory, call.arg(0)?)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// `basic_ofstream::basic_ofstream()`: an output stream with no file attached.
///
/// The single stack argument is MSVC's hidden most-derived flag, which nothing
/// here needs.
fn ofstream_construct(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // A default-constructed stream is not an open one, but it is not in a bad
    // state either; `open` is what decides.
    attach(host, call, true, false, None, 0)?;
    Ok(Return(host.this))
}

/// `basic_ofstream::open(path, mode)`: attaches a file to be written.
///
/// The mode is `ios::out|ios::trunc` at this target's only call site, and
/// truncation is what makes the file hold what this session writes rather than
/// what a previous one left. A second `open` on the same object closes the file
/// it was writing, as the real one does.
fn ofstream_open(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = path_of(call)?;
    let mode = call.arg(1)?;
    let object = host.this;
    let Some(stream) = host.msvcp.streams.get_mut(&object) else {
        return Err(format!(
            "ofstream::open on {object:#010x}, which this host did not construct"
        ));
    };
    stream.binary = mode & BINARY != 0;
    if let Some(handle) = stream.handle.take() {
        host.files.close(handle);
    }
    const CREATE_ALWAYS: u32 = 2;
    match host.files.create(&path, CREATE_ALWAYS) {
        Some(handle) => {
            stream.handle = Some(handle);
            stream.state = 0;
        }
        None => stream.state |= BADBIT,
    }
    Ok(Return(0))
}

/// `basic_ofstream::close()`.
fn ofstream_close(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let (stream, _) = stream_of(host)?;
    if let Some(handle) = stream.handle.take() {
        host.files.close(handle);
    }
    Ok(Return(0))
}

/// `basic_ifstream::basic_ifstream(path, mode)`: a reading stream, opened.
///
/// The third stack argument is the hidden most-derived flag. A file that is not
/// there leaves the stream without one, exactly as the real constructor does --
/// it does not fail the call, so the program checks `is_open` or good() itself.
fn ifstream_construct(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = path_of(call)?;
    let mode = call.arg(1)?;
    let handle = host.files.open(&path);
    if std::env::var("SHIMKIT_LOG_READS").is_ok() && host.notices.len() < 400 {
        host.notice(format!("ifstream {path} mode {mode:#x} -> {}", handle.is_some()));
    }
    let state = if handle.is_some() { 0 } else { BADBIT };
    attach(host, call, false, mode & BINARY != 0, handle, state)?;
    Ok(Return(host.this))
}

/// `is_open()`: whether a file is attached.
fn is_open(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let (stream, _) = stream_of(host)?;
    Ok(Return(u32::from(stream.handle.is_some())))
}

/// `ios_base::eof()`: whether the last read ran off the end.
fn eof(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    let (stream, _) = stream_of(host)?;
    Ok(Return(u32::from(stream.state & EOFLBIT != 0)))
}

/// `basic_istream::getline(buffer, size)`: one line, without its newline, with
/// the terminator after it. The stream this is called on is the `basic_istream`
/// subobject of an `ifstream`, which VC6 puts at the object's own address.
///
/// In text mode a file's CRLF arrives as one newline, which is the runtime's own
/// conversion: a line of a Windows-written log reads without its carriage return.
fn getline(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let buffer = call.arg(0)?;
    let size = call.arg(1)? as i32;
    let (stream, object) = stream_of(host)?;
    let binary = stream.binary;
    let Some(handle) = stream.handle else {
        raise(host, BADBIT);
        call.write(buffer, &[0])?;
        return Ok(Return(object));
    };
    let room = size.max(1) as usize - 1;
    let mut text = Vec::new();
    let mut ended = false;
    while text.len() < room {
        match host.files.read(handle, 1) {
            Some(bytes) if !bytes.is_empty() => {
                if bytes[0] == b'\n' {
                    break;
                }
                text.push(bytes[0]);
            }
            // A read past the end, or a handle that is no longer open: either
            // way the extraction stops and the stream says it was at the end.
            _ => {
                ended = true;
                break;
            }
        }
    }
    if !binary && text.last() == Some(&b'\r') {
        text.pop();
    }
    text.push(0);
    call.write(buffer, &text)?;
    if ended {
        raise(host, EOFLBIT);
    }
    Ok(Return(object))
}

/// The deleting destructor of either stream class: closes the file and forgets
/// the object, so a later write to the same address is not mistaken for a live
/// stream.
fn stream_destruct(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    if let Some(stream) = host.msvcp.streams.remove(&host.this) {
        if let Some(handle) = stream.handle {
            host.files.close(handle);
        }
    }
    Ok(Return(0))
}

/// `operator<<(ostream&, const char*)`: the bytes up to the NUL, through the
/// stream's text or binary mode.
///
/// The stream is the first argument, which for an `ofstream` *is* the object:
/// VC6 puts `basic_ostream` at offset zero of it, which the call sites confirm by
/// loading the object address, adding `0x40` and using that as `this` for
/// `open`/`close` but not for the operator.
fn write_text(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let object = call.arg(0)?;
    let text = cstr_bytes(call.memory, call.arg(1)?)?;
    write_to(host, object, &text)?;
    Ok(Return(object))
}

/// `operator<<(ostream&, manipulator)`.
///
/// The manipulator here is `std::endl`, which every call site passes by address.
/// That address holds a host stub: the import is a pointer to a function, not a
/// function, so this must not call it -- it looks the stub up in the target's
/// table and asks what it is. `endl` puts a newline on the stream and flushes;
/// the newline is the part that reaches a file, and every write here is already
/// a flush.
fn write_manipulator(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let object = host.this;
    let manipulator = call.arg(0)?;
    match crate::cpu::host_slot_at(manipulator, host.target.table) {
        Some(index) => {
            let name = host.target.table.slots[index].name;
            if name.starts_with("?endl@") {
                write_to(host, object, b"\n")?;
                Ok(Return(object))
            } else {
                Err(format!(
                    "operator<<(ostream&, {name}): this host cannot call that manipulator"
                ))
            }
        }
        None => Err(format!(
            "operator<<(ostream&, {manipulator:#010x}): not a host stub, so not a manipulator this host can identify"
        )),
    }
}

/// `std::endl(ostream&)`, reached directly rather than through `operator<<`:
/// a newline, then a flush that is already the case here.
fn endl(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let object = call.arg(0)?;
    write_to(host, object, b"\n")?;
    Ok(Return(object))
}

/// Writes to the stream at `object`, whether or not it has a file.
///
/// An output stream with no file is not an error the program should see: C++ sets
/// `badbit` and carries on, and a run that stopped here would be stopping on a
/// log line.
fn write_to(host: &mut Host, object: u32, bytes: &[u8]) -> Result<()> {
    let Some(stream) = host.msvcp.streams.get_mut(&object) else {
        return Err(format!(
            "write to {object:#010x}, which this host did not construct as a stream"
        ));
    };
    let handle = stream.handle.filter(|_| stream.writing);
    let Some(handle) = handle else {
        stream.state |= BADBIT;
        return Ok(());
    };
    // Text mode is where a file written on Windows gets its CRLF.
    let bytes = if stream.binary || !bytes.contains(&b'\n') {
        std::borrow::Cow::Borrowed(bytes)
    } else {
        let mut expanded = Vec::with_capacity(bytes.len() + 8);
        for byte in bytes {
            if *byte == b'\n' {
                expanded.push(b'\r');
            }
            expanded.push(*byte);
        }
        std::borrow::Cow::Owned(expanded)
    };
    if host.files.write(handle, &bytes).is_none() {
        stream.state |= BADBIT;
    }
    Ok(())
}

// ---- the rest -----------------------------------------------------------

/// `_Lockit`'s constructor and destructor: the runtime's own lock, which a
/// single-threaded machine never contends.
fn lockit(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// Fills the IAT slots that hold a C++ runtime *variable*.
///
/// `basic_string::npos` is a `static const size_type`, so the slot holds the
/// address of the value, and the guest loads it before every `find` and `substr`.
pub fn fill_data_slots(
    host: &mut Host,
    memory: &mut GuestMemory,
    table: crate::dispatch::Table,
) -> Result<()> {
    for slot in table.slots.iter() {
        if !slot.dll.eq_ignore_ascii_case(MSVCP60) {
            continue;
        }
        if slot.name.starts_with("?npos@?$basic_string@") {
            let at = match host.msvcp.npos {
                Some(at) => at,
                None => {
                    let at = host.allocate(4, 4)?;
                    host.msvcp.npos = Some(at);
                    at
                }
            };
            memory.write_u32(at, NPOS)?;
            memory.write_import(slot.iat_va, at)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{HEAP_BASE, STACK_BASE};

    /// A host, memory to put strings and their characters in, and an object to
    /// hold one string.
    struct Machine {
        host: Host,
        memory: GuestMemory,
        string: u32,
        args: u32,
    }

    impl Machine {
        fn new() -> Self {
            Self::with_host(Host::new())
        }

        fn with_host(mut host: Host) -> Self {
            let string = host.allocate(16, 4).expect("the object");
            let mut memory = GuestMemory::empty(HEAP_BASE as usize + 0x1000).expect("blank memory");
            let args = STACK_BASE - 0x200;
            memory.write_bytes(STACK_BASE - 0x400, b"hello\0").unwrap();
            Self {
                host,
                memory,
                string,
                args,
            }
        }

        /// Calls one shim with `this` set and the given stack arguments.
        fn call(
            &mut self,
            shim: crate::shim::Shim,
            values: &[u32],
            arg_bytes: u32,
        ) -> Result<Action> {
            self.call_at(self.string, shim, values, arg_bytes)
        }

        /// Calls one shim on another object, which the streams need: a stream's
        /// `this` is the stream.
        fn call_at(
            &mut self,
            this: u32,
            shim: crate::shim::Shim,
            values: &[u32],
            arg_bytes: u32,
        ) -> Result<Action> {
            for (index, value) in values.iter().enumerate() {
                self.memory
                    .write_u32(self.args + index as u32 * 4, *value)?;
            }
            self.host.this = this;
            let mut call = Call {
                memory: &mut self.memory,
                args: self.args,
                arg_bytes,
            };
            shim(&mut self.host, &mut call)
        }

        fn text(&self) -> String {
            String::from_utf8(content(&self.memory, self.string).unwrap()).unwrap()
        }
    }

    #[test]
    fn a_string_starts_empty_and_holds_what_it_is_assigned() {
        let mut machine = Machine::new();
        machine.call(construct_empty, &[], 0).unwrap();
        assert_eq!(machine.call(length, &[], 0).unwrap(), Return(0));
        assert_eq!(machine.call(is_empty, &[], 0).unwrap(), Return(1));
        // The empty string still has a readable terminator.
        let Return(at) = machine.call(characters, &[], 0).unwrap() else {
            panic!("c_str");
        };
        assert_eq!(machine.memory.read_u8(at).unwrap(), 0);
        assert_eq!(machine.call(end, &[], 0).unwrap(), Return(at), "begin == end");

        let text = machine.memory.read_u32(0 + STACK_BASE - 0x400).unwrap_or(0);
        let _ = text;
        machine
            .call(assign_text, &[STACK_BASE - 0x400], 4)
            .unwrap();
        assert_eq!(machine.text(), "hello");
        assert_eq!(machine.call(length, &[], 0).unwrap(), Return(5));
        assert_eq!(machine.call(is_empty, &[], 0).unwrap(), Return(0));
        let (start, len) = fields(&machine.memory, machine.string).unwrap();
        assert_eq!(machine.call(index, &[1], 4).unwrap(), Return(start + 1));
        assert_eq!(machine.call(end, &[], 0).unwrap(), Return(start + len));
    }

    #[test]
    fn appending_grows_the_block_and_keeps_the_terminator() {
        let mut machine = Machine::new();
        machine.call(construct_from_text, &[STACK_BASE - 0x400], 8).unwrap();
        machine.call(append_character, &[u32::from(b'!')], 4).unwrap();
        // `+=` with another string.
        let other = machine.host.allocate(16, 4).unwrap();
        machine.memory.write_bytes(STACK_BASE - 0x300, b"world\0").unwrap();
        // Build the second string by hand through the shim, then append it.
        machine.host.this = other;
        let mut call = Call {
            memory: &mut machine.memory,
            args: STACK_BASE - 0x200,
            arg_bytes: 8,
        };
        call.write_u32(STACK_BASE - 0x200, STACK_BASE - 0x300).unwrap();
        construct_from_text(&mut machine.host, &mut call).unwrap();
        let item = machine.string;
        machine.call(append_string, &[other], 4).unwrap();
        assert_eq!(machine.text(), "hello!world");
        // The terminator is past the length, inside the block.
        let (at, len) = fields(&machine.memory, item).unwrap();
        assert_eq!(machine.memory.read_u8(at + len).unwrap(), 0);
    }

    #[test]
    fn insert_substr_and_erase_agree_with_each_other() {
        let mut machine = Machine::new();
        machine.call(construct_from_text, &[STACK_BASE - 0x400], 8).unwrap();
        machine.memory.write_bytes(STACK_BASE - 0x300, b"XY\0").unwrap();
        machine.call(insert_text, &[2, STACK_BASE - 0x300], 8).unwrap();
        assert_eq!(machine.text(), "heXYllo");

        // `substr(2, 2)` writes into a buffer the caller owns.
        let result = machine.host.allocate(16, 4).unwrap();
        machine.call(substr, &[result, 2, 2], 12).unwrap();
        assert_eq!(
            String::from_utf8(content(&machine.memory, result).unwrap()).unwrap(),
            "XY"
        );

        machine.call(erase_range, &[0, 4], 8).unwrap();
        assert_eq!(machine.text(), "llo");
        // `erase(where)` cuts to the end and answers the new end.
        let (at, _) = fields(&machine.memory, machine.string).unwrap();
        assert_eq!(
            machine.call(erase_to_end, &[at + 1], 4).unwrap(),
            Return(at + 1)
        );
        assert_eq!(machine.text(), "l");
    }

    #[test]
    fn find_and_the_operators_work_on_bytes() {
        let mut machine = Machine::new();
        machine.call(construct_from_text, &[STACK_BASE - 0x400], 8).unwrap();
        machine.memory.write_bytes(STACK_BASE - 0x300, b"ll\0").unwrap();
        let needle = machine.host.allocate(16, 4).unwrap();
        machine.host.this = needle;
        let mut call = Call {
            memory: &mut machine.memory,
            args: STACK_BASE - 0x200,
            arg_bytes: 8,
        };
        call.write_u32(STACK_BASE - 0x200, STACK_BASE - 0x300).unwrap();
        construct_from_text(&mut machine.host, &mut call).unwrap();

        machine.host.this = machine.string;
        assert_eq!(machine.call(find, &[needle, 0], 8).unwrap(), Return(2));
        assert_eq!(machine.call(find, &[needle, 3], 8).unwrap(), Return(NPOS));
        assert_eq!(machine.call(find, &[needle, 5], 8).unwrap(), Return(NPOS));

        let same = machine.host.allocate(16, 4).unwrap();
        machine.host.this = same;
        let mut call = Call {
            memory: &mut machine.memory,
            args: STACK_BASE - 0x200,
            arg_bytes: 8,
        };
        call.write_u32(STACK_BASE - 0x200, STACK_BASE - 0x400).unwrap();
        construct_from_text(&mut machine.host, &mut call).unwrap();

        assert_eq!(machine.call(equal, &[machine.string, same], 8).unwrap(), Return(1));
        assert_eq!(machine.call(not_equal, &[machine.string, same], 8).unwrap(), Return(0));
        assert_eq!(
            machine.call(less, &[machine.string, same], 8).unwrap(),
            Return(0)
        );
        assert_eq!(
            machine.call(equal_text, &[machine.string, STACK_BASE - 0x400], 8).unwrap(),
            Return(1)
        );

        // `+` returns through the caller's buffer, which is its first argument.
        let joined = machine.host.allocate(16, 4).unwrap();
        machine.call(concat_string_text, &[joined, machine.string, STACK_BASE - 0x300], 12)
            .unwrap();
        assert_eq!(
            String::from_utf8(content(&machine.memory, joined).unwrap()).unwrap(),
            "helloll"
        );
    }

    /// A target whose import table has one slot, so a manipulator's address can
    /// be resolved the way the game's own is.
    static MANIPULATOR: &[crate::dispatch::Slot] = &[crate::dispatch::Slot {
        dll: MSVCP60,
        name: "?endl@std@@YAAAV?$basic_ostream@DU?$char_traits@D@std@@@1@AAV21@@Z",
        iat_va: 0x0048_0000,
        args: Some(4),
        sites: 1,
        agreement: 1,
        ambiguous: 0,
    }];
    static TARGET: crate::target::Target = crate::target::Target {
        name: "test",
        exe_name: "program.exe",
        screen: (640, 480, 8),
        table: crate::dispatch::Table { slots: MANIPULATOR },
        shims: crate::shim::TABLES,
        provided_files: &[],
    };

    fn stream_machine() -> Machine {
        Machine::with_host(Host::with_target(&TARGET))
    }

    #[test]
    fn an_output_stream_writes_its_lines_to_the_file() {
        let mut machine = stream_machine();
        let stream = machine.host.allocate(0x90, 4).unwrap();
        // The hidden most-derived flag is the only stack argument.
        machine
            .call_at(stream, ofstream_construct, &[1], 4)
            .expect("an ofstream is constructed");

        // What the virtual-base call sites need: a table pointer in the object,
        // with a readable entry behind it. The guest computes `object + table[1]`
        // and calls `eof` on that; here the base is the object.
        let table_pointer = machine.memory.read_u32(stream).unwrap();
        assert_ne!(table_pointer, 0, "the object carries a table pointer");
        assert_eq!(machine.memory.read_u32(table_pointer + 4).unwrap(), 0);

        let path = STACK_BASE - 0x300;
        machine.memory.write_bytes(path, b"game.log\0").unwrap();
        machine
            .call_at(stream, ofstream_open, &[path, 1], 8)
            .expect("the file opens");
        assert_eq!(machine.host.files.contents("game.log"), Some(&b""[..]));

        machine.memory.write_bytes(STACK_BASE - 0x400, b"line one\0").unwrap();
        machine
            .call_at(stream, write_text, &[stream, STACK_BASE - 0x400], 8)
            .unwrap();
        // `std::endl` is a pointer to one of the host's own stubs.
        let endl = crate::cpu::HOST_FUNCTION_BASE;
        machine
            .call_at(stream, write_manipulator, &[endl], 4)
            .expect("endl writes the newline");
        machine.memory.write_bytes(STACK_BASE - 0x400, b"line two\0").unwrap();
        machine
            .call_at(stream, write_text, &[stream, STACK_BASE - 0x400], 8)
            .unwrap();
        // The open mode is `out|trunc`, so the file is text: every newline the
        // program wrote is in the file as CRLF.
        assert_eq!(
            machine.host.files.contents("game.log"),
            Some(&b"line one\r\nline two"[..])
        );

        // The destructor closes the file and forgets the object.
        machine.call_at(stream, stream_destruct, &[], 0).unwrap();
        assert!(machine.host.msvcp.streams.is_empty());
        assert_eq!(
            machine.host.files.contents("game.log"),
            Some(&b"line one\r\nline two"[..]),
            "the bytes are in the file, not in the stream"
        );

        // `ios::binary` is the mode that says the bytes are the bytes.
        let raw = machine.host.allocate(0x90, 4).unwrap();
        machine.call_at(raw, ofstream_construct, &[1], 4).unwrap();
        machine.memory.write_bytes(path, b"raw.bin\0").unwrap();
        machine.call_at(raw, ofstream_open, &[path, 0x32], 8).unwrap();
        machine.memory.write_bytes(STACK_BASE - 0x400, b"a\nb\0").unwrap();
        machine
            .call_at(raw, write_text, &[raw, STACK_BASE - 0x400], 8)
            .unwrap();
        assert_eq!(machine.host.files.contents("raw.bin"), Some(&b"a\nb"[..]));
    }

    #[test]
    fn an_input_stream_reads_lines_and_reports_the_end() {
        let mut machine = stream_machine();
        machine
            .host
            .files
            .provide("game.log", b"first\r\nsecond\ntail".to_vec());
        let stream = machine.host.allocate(0x90, 4).unwrap();
        let path = STACK_BASE - 0x300;
        machine.memory.write_bytes(path, b"game.log\0").unwrap();
        // `path`, the mode, and the hidden flag.
        machine
            .call_at(stream, ifstream_construct, &[path, 1, 1], 12)
            .unwrap();
        assert_eq!(machine.call_at(stream, is_open, &[], 0).unwrap(), Return(1));

        let buffer = STACK_BASE - 0x800;
        let line = |machine: &mut Machine| -> String {
            machine.call_at(stream, getline, &[buffer, 64], 8).unwrap();
            machine.memory.read_cstr(buffer).unwrap()
        };
        assert_eq!(line(&mut machine), "first", "text mode takes the CR with the LF");
        assert_eq!(machine.call_at(stream, eof, &[], 0).unwrap(), Return(0));
        assert_eq!(line(&mut machine), "second");
        assert_eq!(machine.call_at(stream, eof, &[], 0).unwrap(), Return(0));
        // The last line has no newline, so reaching the end of it is the end.
        assert_eq!(line(&mut machine), "tail");
        assert_eq!(machine.call_at(stream, eof, &[], 0).unwrap(), Return(1));
        // And once past the end, an extraction is empty and still at the end.
        assert_eq!(line(&mut machine), "");
        assert_eq!(machine.call_at(stream, eof, &[], 0).unwrap(), Return(1));
        machine.call_at(stream, stream_destruct, &[], 0).unwrap();
    }

    #[test]
    fn a_missing_file_opens_no_stream_and_a_write_to_nothing_is_bad_bits() {
        let mut machine = stream_machine();
        let stream = machine.host.allocate(0x90, 4).unwrap();
        let path = STACK_BASE - 0x300;
        machine.memory.write_bytes(path, b"absent.log\0").unwrap();
        machine
            .call_at(stream, ifstream_construct, &[path, 1, 1], 12)
            .unwrap();
        assert_eq!(machine.call_at(stream, is_open, &[], 0).unwrap(), Return(0));
        assert_eq!(machine.call_at(stream, eof, &[], 0).unwrap(), Return(0));

        // An output stream that never opened takes the bytes and sets badbit,
        // rather than stopping a run over a log line.
        let out = machine.host.allocate(0x90, 4).unwrap();
        machine
            .call_at(out, ofstream_construct, &[1], 4)
            .unwrap();
        machine.memory.write_bytes(STACK_BASE - 0x400, b"lost\0").unwrap();
        machine
            .call_at(out, write_text, &[out, STACK_BASE - 0x400], 8)
            .expect("a write to an unopened stream is not a fault");
        assert_ne!(machine.host.msvcp.streams[&out].state & BADBIT, 0);
    }

    #[test]
    fn the_destructor_gives_the_block_back() {
        let mut machine = Machine::new();
        machine.call(construct_from_text, &[STACK_BASE - 0x400], 8).unwrap();
        let (at, _) = fields(&machine.memory, machine.string).unwrap();
        assert!(machine.host.allocated(at).is_some(), "the characters are owned");
        machine.call(destruct, &[], 0).unwrap();
        assert_eq!(machine.host.allocated(at), None, "and released");
        // A destructed string is empty rather than dangling.
        assert_eq!(machine.text(), "");
    }
}
