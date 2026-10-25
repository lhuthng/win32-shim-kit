//! The `KERNEL32.dll` subset the boot needs, written against the guest's own
//! calling convention.
//!
//! Each entry declares its argument bytes, because the dispatch table only knows
//! an arity for the slots whose call sites agreed; the session uses the declared
//! value to pop the arguments and cross-checks it against the table wherever the
//! table has an opinion.
//!
//! Two behaviours are deliberate and worth naming:
//!
//! - The **heap arena is bump-with-reuse**. `HeapFree` and `VirtualFree` return
//!   a block to a free list that the next allocation reuses best-fit, so the
//!   game's frame-to-frame alloc/free churn does not consume the reserve. A
//!   block that is really leaked still grows the arena, which the session
//!   reports as `heap_used`. A request the arena cannot hold answers `NULL`
//!   (the way Windows does, so the guest's own out-of-memory path runs) and is
//!   recorded as a notice naming the size and the guest call site -- an
//!   impossible size is a corrupted length upstream, not a full heap.
//! - **Text is Latin-1 end to end.** `MultiByteToWideChar` and its siblings map
//!   bytes to code points one for one. That round-trips every byte the game
//!   stores, which is what matters for a single-byte localization; it is not a
//!   real codepage table.
//!
//! Anything missing stays unimplemented on purpose: the session stops and names
//! the slot, which is the diagnostic that drives this file's growth.

use crate::pe::IMAGE_BASE;
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::{Exit, Return};

const KERNEL32: &str = "KERNEL32.dll";

/// Windows XP, 5.1 build 2600. The game predates the browser, so claiming a
/// 2001-era platform keeps its version checks on the branch the art was drawn
/// for; the localization's own volume fix targets newer versions separately.
const WINDOWS_VERSION: u32 = 0x0a28_0105;

const STD_INPUT_HANDLE: i32 = -10;
const STD_OUTPUT_HANDLE: i32 = -11;
const STD_ERROR_HANDLE: i32 = -12;
const STD_INPUT: u32 = 0x11;
const STD_OUTPUT: u32 = 0x12;
const STD_ERROR: u32 = 0x13;

const FILE_TYPE_CHAR: u32 = 2;
const INVALID_HANDLE_VALUE: u32 = 0xffff_ffff;

const C1_UPPER: u16 = 0x0001;
const C1_LOWER: u16 = 0x0002;
const C1_DIGIT: u16 = 0x0004;
const C1_SPACE: u16 = 0x0008;
const C1_PUNCT: u16 = 0x0010;
const C1_CNTRL: u16 = 0x0020;
const C1_BLANK: u16 = 0x0040;
const C1_ALPHA: u16 = 0x0100;

const LCMAP_LOWERCASE: u32 = 0x0000_0100;
const LCMAP_UPPERCASE: u32 = 0x0000_0200;
const NORM_IGNORECASE: u32 = 0x0000_0001;

pub const SHIMS: &[Binding] = &[
    Binding { dll: KERNEL32, name: "Beep", arg_bytes: 8, shim: beep },
    Binding { dll: KERNEL32, name: "CloseHandle", arg_bytes: 4, shim: close_handle },
    Binding { dll: KERNEL32, name: "CompareStringA", arg_bytes: 24, shim: compare_string_a },
    Binding { dll: KERNEL32, name: "CompareStringW", arg_bytes: 24, shim: compare_string_w },
    Binding { dll: KERNEL32, name: "ExitProcess", arg_bytes: 4, shim: exit_process },
    Binding { dll: KERNEL32, name: "FileTimeToLocalFileTime", arg_bytes: 8, shim: file_time_to_local_file_time },
    Binding { dll: KERNEL32, name: "FileTimeToSystemTime", arg_bytes: 8, shim: file_time_to_system_time },
    Binding { dll: KERNEL32, name: "FreeEnvironmentStringsA", arg_bytes: 4, shim: free_environment_strings },
    Binding { dll: KERNEL32, name: "FreeEnvironmentStringsW", arg_bytes: 4, shim: free_environment_strings },
    Binding { dll: KERNEL32, name: "GetACP", arg_bytes: 0, shim: get_acp },
    Binding { dll: KERNEL32, name: "GetCPInfo", arg_bytes: 8, shim: get_cp_info },
    Binding { dll: KERNEL32, name: "GetCommandLineA", arg_bytes: 0, shim: get_command_line },
    Binding { dll: KERNEL32, name: "GetCurrentProcess", arg_bytes: 0, shim: get_current_process },
    Binding { dll: KERNEL32, name: "GetEnvironmentStrings", arg_bytes: 0, shim: get_environment_strings },
    Binding { dll: KERNEL32, name: "GetEnvironmentStringsW", arg_bytes: 0, shim: get_environment_strings_w },
    Binding { dll: KERNEL32, name: "GetFileType", arg_bytes: 4, shim: get_file_type },
    Binding { dll: KERNEL32, name: "GetLastError", arg_bytes: 0, shim: get_last_error },
    Binding { dll: KERNEL32, name: "GetLocalTime", arg_bytes: 4, shim: get_local_time },
    Binding { dll: KERNEL32, name: "GetLogicalDrives", arg_bytes: 0, shim: get_logical_drives },
    Binding { dll: KERNEL32, name: "GetModuleFileNameA", arg_bytes: 12, shim: get_module_file_name },
    Binding { dll: KERNEL32, name: "GetModuleHandleA", arg_bytes: 4, shim: get_module_handle },
    Binding { dll: KERNEL32, name: "GetOEMCP", arg_bytes: 0, shim: get_oem_cp },
    Binding { dll: KERNEL32, name: "GetProfileStringA", arg_bytes: 20, shim: get_profile_string },
    Binding { dll: KERNEL32, name: "GetProcAddress", arg_bytes: 8, shim: get_proc_address },
    Binding { dll: KERNEL32, name: "GetStartupInfoA", arg_bytes: 4, shim: get_startup_info },
    Binding { dll: KERNEL32, name: "GetStdHandle", arg_bytes: 4, shim: get_std_handle },
    Binding { dll: KERNEL32, name: "GetStringTypeA", arg_bytes: 20, shim: get_string_type_a },
    Binding { dll: KERNEL32, name: "GetStringTypeW", arg_bytes: 16, shim: get_string_type_w },
    Binding { dll: KERNEL32, name: "GetSystemTime", arg_bytes: 4, shim: get_system_time },
    Binding { dll: KERNEL32, name: "GetTimeZoneInformation", arg_bytes: 4, shim: get_time_zone_information },
    Binding { dll: KERNEL32, name: "GetVersion", arg_bytes: 0, shim: get_version },
    Binding { dll: KERNEL32, name: "GlobalLock", arg_bytes: 4, shim: global_lock },
    Binding { dll: KERNEL32, name: "GlobalUnlock", arg_bytes: 4, shim: global_unlock },
    Binding { dll: KERNEL32, name: "HeapAlloc", arg_bytes: 12, shim: heap_alloc },
    Binding { dll: KERNEL32, name: "HeapCreate", arg_bytes: 12, shim: heap_create },
    Binding { dll: KERNEL32, name: "HeapDestroy", arg_bytes: 4, shim: heap_destroy },
    Binding { dll: KERNEL32, name: "HeapFree", arg_bytes: 12, shim: heap_free },
    Binding { dll: KERNEL32, name: "HeapReAlloc", arg_bytes: 16, shim: heap_realloc },
    Binding { dll: KERNEL32, name: "IsBadCodePtr", arg_bytes: 4, shim: is_bad_pointer },
    Binding { dll: KERNEL32, name: "IsBadReadPtr", arg_bytes: 8, shim: is_bad_pointer },
    Binding { dll: KERNEL32, name: "IsBadWritePtr", arg_bytes: 8, shim: is_bad_pointer },
    Binding { dll: KERNEL32, name: "LCMapStringA", arg_bytes: 24, shim: lc_map_string_a },
    Binding { dll: KERNEL32, name: "LCMapStringW", arg_bytes: 24, shim: lc_map_string_w },
    Binding { dll: KERNEL32, name: "LoadLibraryA", arg_bytes: 4, shim: load_library },
    Binding { dll: KERNEL32, name: "MultiByteToWideChar", arg_bytes: 24, shim: multi_byte_to_wide },
    Binding { dll: KERNEL32, name: "RaiseException", arg_bytes: 16, shim: raise_exception },
    Binding { dll: KERNEL32, name: "RtlUnwind", arg_bytes: 16, shim: rtl_unwind },
    Binding { dll: KERNEL32, name: "SetEnvironmentVariableA", arg_bytes: 8, shim: set_environment_variable },
    Binding { dll: KERNEL32, name: "SetHandleCount", arg_bytes: 4, shim: set_handle_count },
    Binding { dll: KERNEL32, name: "SetStdHandle", arg_bytes: 8, shim: set_std_handle },
    Binding { dll: KERNEL32, name: "SetUnhandledExceptionFilter", arg_bytes: 4, shim: set_unhandled_exception_filter },
    Binding { dll: KERNEL32, name: "Sleep", arg_bytes: 4, shim: sleep },
    Binding { dll: KERNEL32, name: "TerminateProcess", arg_bytes: 8, shim: terminate_process },
    Binding { dll: KERNEL32, name: "UnhandledExceptionFilter", arg_bytes: 4, shim: unhandled_exception_filter },
    Binding { dll: KERNEL32, name: "VirtualAlloc", arg_bytes: 16, shim: virtual_alloc },
    Binding { dll: KERNEL32, name: "VirtualFree", arg_bytes: 12, shim: virtual_free },
    Binding { dll: KERNEL32, name: "WideCharToMultiByte", arg_bytes: 32, shim: wide_to_multi_byte },
    Binding { dll: KERNEL32, name: "lstrcmpiA", arg_bytes: 8, shim: lstrcmpi },
    Binding { dll: KERNEL32, name: "lstrcpyA", arg_bytes: 8, shim: lstrcpy },
];

// ---- process and version -------------------------------------------------

fn get_version(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(WINDOWS_VERSION))
}

fn get_last_error(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.last_error))
}

fn get_current_process(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(INVALID_HANDLE_VALUE))
}

fn get_command_line(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.command_line(call.memory)?))
}

fn get_module_handle(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(IMAGE_BASE))
}

fn get_module_file_name(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let buffer = call.arg(1)?;
    let capacity = call.arg(2)?;
    if buffer == 0 || capacity == 0 {
        return Ok(Return(0));
    }
    // The loaded image's own name, from the target: a folder with a split build
    // beside the original must answer with the build the guest actually is.
    let name = host.target.exe_name.as_bytes();
    let written = (capacity as usize - 1).min(name.len());
    call.write(buffer, &name[..written])?;
    call.write(buffer.wrapping_add(written as u32), &[0])?;
    Ok(Return(written as u32))
}

fn get_startup_info(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let info = call.arg(0)?;
    if info != 0 {
        // STARTUPINFOA is 68 bytes on every 32-bit Windows; an empty one tells
        // the CRT there is no inherited console or window.
        call.zero(info, 68)?;
        call.write_u32(info, 68)?;
    }
    Ok(Return(0))
}

fn exit_process(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Exit(call.arg(0)?))
}

fn terminate_process(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Exit(call.arg(1)?))
}

fn set_unhandled_exception_filter(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // There was no previous filter, so the CRT installs its own and never chains.
    Ok(Return(0))
}

fn unhandled_exception_filter(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn raise_exception(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Err(format!(
        "the guest raised a structured exception {:#010x}",
        call.arg(0)?
    ))
}

fn rtl_unwind(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn is_bad_pointer(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The memory layer checks every access anyway; reporting "fine" here only
    // means the CRT's own validation never short-circuits a real access.
    Ok(Return(0))
}

/// The guest's files come from the bundle, which is read-only, and the writable
/// area that will hold saves does not exist yet. So nothing a caller can name has
/// been written by this host, and "no such file" is the truth rather than a
/// placeholder: the CRT's `remove()` maps it to `ENOENT`, which is exactly what a
/// caller cleaning up a temp file that is not there should see.
fn sleep(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The frame clock owns pacing; a blocking sleep here would freeze the page.
    Ok(Return(0))
}

// ---- standard streams ----------------------------------------------------

fn get_std_handle(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(match call.arg(0)? as i32 {
        STD_INPUT_HANDLE => STD_INPUT,
        STD_OUTPUT_HANDLE => STD_OUTPUT,
        STD_ERROR_HANDLE => STD_ERROR,
        _ => 0,
    }))
}

fn set_std_handle(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn set_handle_count(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?))
}

fn get_file_type(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    // A handle the file table owns is a disk file; the standard streams are the
    // console, which is why they answer `FILE_TYPE_CHAR`.
    if let Some(kind) = crate::shim::files::file_type(host, handle) {
        return Ok(Return(kind));
    }
    Ok(Return(if (STD_INPUT..=STD_ERROR).contains(&handle) {
        FILE_TYPE_CHAR
    } else {
        0
    }))
}

fn close_handle(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Closing a file handle drops it from the table. Every other handle in this
    // host is a number with no resource behind it, so the close succeeds and the
    // number is simply forgotten.
    crate::shim::files::close(host, call.arg(0)?);
    Ok(Return(1))
}

// ---- heap and virtual memory --------------------------------------------

fn heap_create(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // Every heap is served by the one arena, so the handle only has to be
    // distinct and non-null.
    Ok(Return(host.new_heap()))
}

fn heap_destroy(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn heap_alloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let size = call.arg(2)?;
    match host.allocate(size, 8) {
        Ok(pointer) => Ok(Return(pointer)),
        Err(error) => Ok(Return(refuse(host, call, "HeapAlloc", size, error))),
    }
}

fn heap_realloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let pointer = call.arg(2)?;
    let size = call.arg(3)?;
    match host.reallocate(call.memory, pointer, size, 8) {
        Ok(pointer) => Ok(Return(pointer)),
        // The old block is untouched when the move fails, as on Windows.
        Err(error) => Ok(Return(refuse(host, call, "HeapReAlloc", size, error))),
    }
}

/// Answers a request the arena cannot satisfy the way Windows does: `NULL` (for
/// the heap) after recording why, instead of ending the session.
///
/// A 1998 game ran on a 32 MB machine, so it already has an out-of-memory path
/// -- and it is the host's job to hand it the null the path tests for. The call
/// site sits on the stack below the arguments, and naming it is what turns
/// "out of memory" into a place to look; a 1.1 GB request out of a 40 KB sound
/// is a corrupted length upstream, not a full heap.
fn refuse(host: &mut Host, call: &Call<'_>, what: &str, size: u32, error: String) -> u32 {
    let caller = call.memory.read_u32(call.args.wrapping_sub(4)).unwrap_or(0);
    host.notice(format!(
        "{what} of {size} bytes from {caller:#010x} failed: {error}"
    ));
    0
}

fn heap_free(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.release(call.arg(2)?);
    Ok(Return(1))
}

fn virtual_alloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    let size = call.arg(1)?;
    let allocated = if address == 0 {
        host.allocate(size, 0x1000)
    } else {
        host.allocate_at(address, size)
    };
    match allocated {
        Ok(pointer) => Ok(Return(pointer)),
        Err(error) => Ok(Return(refuse(host, call, "VirtualAlloc", size, error))),
    }
}

fn virtual_free(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.release(call.arg(0)?);
    Ok(Return(1))
}

// ---- environment and command line ---------------------------------------

fn get_environment_strings(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.environment(call.memory)?))
}

fn get_environment_strings_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.environment_wide(call.memory)?))
}

fn free_environment_strings(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn set_environment_variable(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn get_profile_string(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let buffer = call.arg(3)?;
    let capacity = call.arg(4)?;
    if buffer != 0 && capacity > 0 {
        call.write(buffer, &[0])?;
    }
    Ok(Return(0))
}

fn global_lock(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Every handle the game holds is a fixed pointer, so locking is identity.
    Ok(Return(call.arg(0)?))
}

fn global_unlock(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn get_logical_drives(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn beep(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn load_library(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.last_error = 126; // ERROR_MOD_NOT_FOUND
    Ok(Return(0))
}

fn get_proc_address(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.last_error = 127; // ERROR_PROC_NOT_FOUND
    Ok(Return(0))
}

// ---- strings and codepages ----------------------------------------------

fn get_acp(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1252))
}

fn get_oem_cp(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(437))
}

fn get_cp_info(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let info = call.arg(1)?;
    if info != 0 {
        // CPINFO: dword MaxCharSize, 2 default bytes, 12 lead-byte slots.
        call.zero(info, 18)?;
        call.write_u32(info, 1)?;
        call.write(info.wrapping_add(4), &[b'?', 0])?;
    }
    Ok(Return(1))
}

/** Reads a NUL-terminated byte string without decoding it.
///
/// The guest's text is single-byte and not UTF-8, so a `String` would replace
/// every accented byte. Everything that copies or compares guest text works on
/// bytes for that reason.
*/
fn read_bytes_until_nul(memory: &crate::memory::GuestMemory, source: u32) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for offset in 0..4096_u32 {
        let byte = memory.read_u8(source.wrapping_add(offset))?;
        if byte == 0 {
            return Ok(bytes);
        }
        bytes.push(byte);
    }
    Err(format!("string at {source:#010x} is not terminated"))
}

fn lstrcpy(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let source = call.arg(1)?;
    let mut bytes = read_bytes_until_nul(call.memory, source)?;
    bytes.push(0);
    call.write(destination, &bytes)?;
    Ok(Return(destination))
}

fn lstrcmpi(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let left = read_bytes_until_nul(call.memory, call.arg(0)?)?;
    let right = read_bytes_until_nul(call.memory, call.arg(1)?)?;
    let left = left.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    let right = right.iter().map(u8::to_ascii_lowercase).collect::<Vec<_>>();
    Ok(Return(match left.cmp(&right) {
        std::cmp::Ordering::Less => u32::MAX,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

fn compare_string_a(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let ignore_case = call.arg(1)? & NORM_IGNORECASE != 0;
    let left = read_ansi(call, call.arg(2)?, call.arg(3)?)?;
    let right = read_ansi(call, call.arg(4)?, call.arg(5)?)?;
    Ok(compare(&left, &right, ignore_case))
}

fn compare_string_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let ignore_case = call.arg(1)? & NORM_IGNORECASE != 0;
    let left = read_wide(call, call.arg(2)?, call.arg(3)?)?;
    let right = read_wide(call, call.arg(4)?, call.arg(5)?)?;
    Ok(compare(&left, &right, ignore_case))
}

fn compare(left: &str, right: &str, ignore_case: bool) -> Action {
    let ordering = if ignore_case {
        left.to_lowercase().cmp(&right.to_lowercase())
    } else {
        left.cmp(right)
    };
    Return(match ordering {
        std::cmp::Ordering::Less => 1,
        std::cmp::Ordering::Equal => 2,
        std::cmp::Ordering::Greater => 3,
    })
}

fn lc_map_string_a(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let flags = call.arg(1)?;
    let text = read_ansi(call, call.arg(2)?, call.arg(3)?)?;
    map_string(call, flags, call.arg(4)?, call.arg(5)?, &text, false)
}

fn lc_map_string_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let flags = call.arg(1)?;
    let text = read_wide(call, call.arg(2)?, call.arg(3)?)?;
    map_string(call, flags, call.arg(4)?, call.arg(5)?, &text, true)
}

fn map_string(
    call: &mut Call<'_>,
    flags: u32,
    destination: u32,
    capacity: u32,
    text: &str,
    wide: bool,
) -> Result<Action> {
    let mapped = if flags & LCMAP_LOWERCASE != 0 {
        text.to_lowercase()
    } else if flags & LCMAP_UPPERCASE != 0 {
        text.to_uppercase()
    } else {
        text.to_string()
    };
    let units: Vec<u32> = mapped.chars().map(u32::from).collect();
    let count = units.len() as u32;
    if destination == 0 || capacity == 0 {
        return Ok(Return(count));
    }
    if capacity < count {
        return Ok(Return(0));
    }
    for (index, unit) in units.iter().enumerate() {
        let at = destination + index as u32 * if wide { 2 } else { 1 };
        if wide {
            call.write_u16(at, *unit as u16)?;
        } else {
            call.write(at, &[*unit as u8])?;
        }
    }
    Ok(Return(count))
}

fn get_string_type_a(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let flags = call.arg(1)?;
    let source = call.arg(2)?;
    let count = call.arg(3)?;
    let destination = call.arg(4)?;
    classify_string(call, flags, source, count, destination, false)
}

fn get_string_type_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let flags = call.arg(0)?;
    let source = call.arg(1)?;
    let count = call.arg(2)?;
    let destination = call.arg(3)?;
    if flags != 1 {
        // Only CT_CTYPE1 exists in the CRT's own tables; anything else is a
        // caller bug we would rather see than fake.
        return Ok(Return(0));
    }
    classify_string(call, flags, source, count, destination, true)
}

fn classify_string(
    call: &mut Call<'_>,
    _flags: u32,
    source: u32,
    count: u32,
    destination: u32,
    wide: bool,
) -> Result<Action> {
    let count = if count == u32::MAX {
        read_cstr_units(call, source, wide)?.len() as u32 + 1
    } else {
        count
    };
    for index in 0..count {
        let at = source + index * if wide { 2 } else { 1 };
        let unit = if wide {
            call.memory.read_u16(at)?
        } else {
            u16::from(call.memory.read_u8(at)?)
        };
        let class = if unit == 0 {
            0
        } else {
            classify(unit)
        };
        call.write_u16(destination + index * 2, class)?;
    }
    Ok(Return(1))
}

fn classify(unit: u16) -> u16 {
    let character = char::from_u32(u32::from(unit)).unwrap_or('\u{fffd}');
    let mut class = 0;
    if character.is_ascii_uppercase() {
        class |= C1_UPPER | C1_ALPHA;
    }
    if character.is_ascii_lowercase() {
        class |= C1_LOWER | C1_ALPHA;
    }
    if character.is_ascii_digit() {
        class |= C1_DIGIT;
    }
    if character.is_ascii_whitespace() {
        class |= C1_SPACE;
    }
    if character == ' ' {
        class |= C1_BLANK | C1_SPACE;
    }
    if character.is_ascii_punctuation() {
        class |= C1_PUNCT;
    }
    if character.is_ascii_control() {
        class |= C1_CNTRL;
    }
    class
}

fn multi_byte_to_wide(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let source = call.arg(2)?;
    let source_length = call.arg(3)? as i32;
    let destination = call.arg(4)?;
    let capacity = call.arg(5)? as i32;

    let bytes = if source_length < 0 {
        let mut bytes = read_bytes_until_nul(call.memory, source)?;
        bytes.push(0);
        bytes
    } else {
        call.memory.read_bytes(source, source_length as usize)?.to_vec()
    };
    let count = bytes.len() as u32;
    if destination == 0 || capacity == 0 {
        return Ok(Return(count));
    }
    if capacity < count as i32 {
        return Ok(Return(0));
    }
    for (index, byte) in bytes.iter().enumerate() {
        call.write_u16(destination + index as u32 * 2, u16::from(*byte))?;
    }
    Ok(Return(count))
}

fn wide_to_multi_byte(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let source = call.arg(2)?;
    let source_length = call.arg(3)? as i32;
    let destination = call.arg(4)?;
    let capacity = call.arg(5)? as i32;

    let units = if source_length < 0 {
        let mut units = read_cstr_units(call, source, true)?;
        units.push(0);
        units
    } else {
        let mut units = Vec::with_capacity(source_length as usize);
        for index in 0..source_length as u32 {
            units.push(call.memory.read_u16(source + index * 2)?);
        }
        units
    };
    let count = units.len() as u32;
    if destination == 0 || capacity == 0 {
        return Ok(Return(count));
    }
    if capacity < count as i32 {
        return Ok(Return(0));
    }
    for (index, unit) in units.iter().enumerate() {
        call.write(destination + index as u32, &[(*unit & 0xff) as u8])?;
    }
    Ok(Return(count))
}

fn read_ansi(call: &Call<'_>, source: u32, length: u32) -> Result<String> {
    if source == 0 {
        return Ok(String::new());
    }
    if length == u32::MAX {
        let bytes = read_bytes_until_nul(call.memory, source)?;
        return Ok(String::from_utf8_lossy(&bytes).into_owned());
    }
    let bytes = call.memory.read_bytes(source, length as usize)?;
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn read_wide(call: &Call<'_>, source: u32, length: u32) -> Result<String> {
    if source == 0 {
        return Ok(String::new());
    }
    let units = if length == u32::MAX {
        read_cstr_units(call, source, true)?
    } else {
        let mut units = Vec::with_capacity(length as usize);
        for index in 0..length {
            units.push(call.memory.read_u16(source + index * 2)?);
        }
        units
    };
    Ok(String::from_utf16_lossy(&units))
}

fn read_cstr_units(call: &Call<'_>, source: u32, wide: bool) -> Result<Vec<u16>> {
    let mut units = Vec::new();
    for index in 0..4096_u32 {
        let at = source + index * if wide { 2 } else { 1 };
        let unit = if wide {
            call.memory.read_u16(at)?
        } else {
            u16::from(call.memory.read_u8(at)?)
        };
        if unit == 0 {
            return Ok(units);
        }
        units.push(unit);
    }
    Err(format!("string at {source:#010x} is not terminated"))
}

// ---- time ----------------------------------------------------------------

fn write_system_time(call: &mut Call<'_>, at: u32) -> Result<()> {
    if at == 0 {
        return Ok(());
    }
    // A fixed 2000-01-01 12:00:00 keeps runs reproducible; the game only uses
    // this for the date display and a few seeds.
    for (index, value) in [2000_u16, 1, 6, 1, 12, 0, 0, 0].iter().enumerate() {
        call.write_u16(at + index as u32 * 2, *value)?;
    }
    Ok(())
}

fn get_system_time(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    write_system_time(call, call.arg(0)?)?;
    Ok(Return(0))
}

fn get_local_time(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    write_system_time(call, call.arg(0)?)?;
    Ok(Return(0))
}

fn get_time_zone_information(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let info = call.arg(0)?;
    if info != 0 {
        call.zero(info, 172)?;
    }
    // TIME_ZONE_ID_UNKNOWN: the guest falls back to the standard offset.
    Ok(Return(0))
}

fn file_time_to_local_file_time(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let source = call.arg(0)?;
    let destination = call.arg(1)?;
    if source != 0 && destination != 0 {
        let value = call.memory.read_u32(source)?;
        let high = call.memory.read_u32(source.wrapping_add(4))?;
        call.write_u32(destination, value)?;
        call.write_u32(destination.wrapping_add(4), high)?;
    }
    Ok(Return(1))
}

fn file_time_to_system_time(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    write_system_time(call, call.arg(1)?)?;
    Ok(Return(1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};
    use crate::pe::IMAGE_BASE;
    use crate::shim::Host;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((
            Host::new(),
            GuestMemory::load(image).expect("image maps"),
        ))
    }

    /// Runs one shim with arguments written at `IMAGE_BASE + 0x1000`, which is
    /// free in the loaded image's DSEG region.
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
        // The stack is writable in a mapped image; .text is not.
        let args = STACK_BASE + 0x1000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes)?;
        let mut call = Call {
            memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call)
    }

    #[test]
    fn reports_windows_xp() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetVersion", &[]).unwrap(),
            Return(WINDOWS_VERSION)
        );
        assert_eq!(WINDOWS_VERSION & 0xff, 5, "major version 5");
    }

    #[test]
    fn the_module_handle_is_the_image_base() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetModuleHandleA", &[0]).unwrap(),
            Return(IMAGE_BASE)
        );
    }

    #[test]
    fn heap_allocations_are_writable_guest_memory() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let heap = match call_shim(&mut host, &mut memory, "HeapCreate", &[0, 0, 0]).unwrap() {
            Return(handle) => handle,
            other => panic!("HeapCreate returned {other:?}"),
        };
        assert_ne!(heap, 0);

        let pointer = match call_shim(&mut host, &mut memory, "HeapAlloc", &[heap, 0, 64]).unwrap() {
            Return(pointer) => pointer,
            other => panic!("HeapAlloc returned {other:?}"),
        };
        assert!(pointer > IMAGE_BASE);
        assert_eq!(memory.read_bytes(pointer, 64).unwrap(), &[0_u8; 64]);
        memory.write_u32(pointer, 0xdead_beef).unwrap();
        assert_eq!(memory.read_u32(pointer).unwrap(), 0xdead_beef);

        let bigger = match call_shim(&mut host, &mut memory, "HeapReAlloc", &[heap, 0, pointer, 128])
            .unwrap()
        {
            Return(pointer) => pointer,
            other => panic!("HeapReAlloc returned {other:?}"),
        };
        assert_eq!(memory.read_u32(bigger).unwrap(), 0xdead_beef, "contents kept");
        assert_eq!(host.allocated(bigger), Some(128));
    }

    #[test]
    fn freed_heap_blocks_are_reused() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let heap = match call_shim(&mut host, &mut memory, "HeapCreate", &[0, 0, 0]).unwrap() {
            Return(handle) => handle,
            other => panic!("HeapCreate returned {other:?}"),
        };
        let pointer = match call_shim(&mut host, &mut memory, "HeapAlloc", &[heap, 0, 64]).unwrap() {
            Return(pointer) => pointer,
            other => panic!("HeapAlloc returned {other:?}"),
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "HeapFree", &[heap, 0, pointer]).unwrap(),
            Return(1)
        );
        let again = match call_shim(&mut host, &mut memory, "HeapAlloc", &[heap, 0, 64]).unwrap() {
            Return(pointer) => pointer,
            other => panic!("HeapAlloc returned {other:?}"),
        };
        assert_eq!(again, pointer, "the freed block is reused, not appended");
        assert_eq!(host.allocated(again), Some(64));
    }

    #[test]
    fn heap_realloc_releases_the_old_block() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let heap = match call_shim(&mut host, &mut memory, "HeapCreate", &[0, 0, 0]).unwrap() {
            Return(handle) => handle,
            other => panic!("HeapCreate returned {other:?}"),
        };
        let pointer = match call_shim(&mut host, &mut memory, "HeapAlloc", &[heap, 0, 64]).unwrap() {
            Return(pointer) => pointer,
            other => panic!("HeapAlloc returned {other:?}"),
        };
        let bigger = match call_shim(&mut host, &mut memory, "HeapReAlloc", &[heap, 0, pointer, 128])
            .unwrap()
        {
            Return(pointer) => pointer,
            other => panic!("HeapReAlloc returned {other:?}"),
        };
        assert_eq!(host.allocated(pointer), None, "the moved-from block is free");
        assert_eq!(host.allocated(bigger), Some(128));
    }

    #[test]
    fn a_request_the_arena_cannot_hold_answers_null() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let heap = match call_shim(&mut host, &mut memory, "HeapCreate", &[0, 0, 0]).unwrap() {
            Return(handle) => handle,
            other => panic!("HeapCreate returned {other:?}"),
        };
        // The size a corrupted WAV length produces: 1.1 GB, which no arena can
        // hold. Windows answers NULL; the session has to do the same instead of
        // faulting, because the guest tests for it.
        assert_eq!(
            call_shim(&mut host, &mut memory, "HeapAlloc", &[heap, 0, 0x4646_4960]).unwrap(),
            Return(0)
        );
        // The refusal leaves the heap usable.
        let pointer = match call_shim(&mut host, &mut memory, "HeapAlloc", &[heap, 0, 64]).unwrap() {
            Return(pointer) => pointer,
            other => panic!("HeapAlloc returned {other:?}"),
        };
        assert!(pointer > IMAGE_BASE);
        assert_eq!(host.allocated(pointer), Some(64));
        assert_eq!(
            call_shim(&mut host, &mut memory, "VirtualAlloc", &[0, 0x4646_4960, 0x1000, 4])
                .unwrap(),
            Return(0)
        );
    }

    #[test]
    fn virtual_alloc_honours_an_explicit_address() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let address = 0x0140_0000;
        assert_eq!(
            call_shim(&mut host, &mut memory, "VirtualAlloc", &[address, 0x2000, 0x1000, 4])
                .unwrap(),
            Return(address)
        );
        memory.write_u8(address, 7).unwrap();
        assert_eq!(memory.read_u8(address).unwrap(), 7);
    }

    #[test]
    fn strings_copy_and_compare() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let source = STACK_BASE + 0x2000;
        let destination = STACK_BASE + 0x2100;
        memory.write_bytes(source, b"Ho\xe0\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "lstrcpyA", &[destination, source]).unwrap(),
            Return(destination)
        );
        assert_eq!(
            memory.read_bytes(destination, 4).unwrap(),
            b"Ho\xe0\0",
            "text is copied byte for byte, not decoded"
        );

        memory.write_bytes(source, b"Alpha\0").unwrap();
        memory.write_bytes(source + 0x10, b"alpha\0").unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "lstrcmpiA", &[source, source + 0x10]).unwrap(),
            Return(0)
        );
    }

    #[test]
    fn the_codepage_maps_bytes_to_code_points_one_for_one() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let source = STACK_BASE + 0x3000;
        let destination = STACK_BASE + 0x3100;
        memory.write_bytes(source, b"Vi\xea\xf2t\0").unwrap();

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "MultiByteToWideChar",
                &[1252, 0, source, u32::MAX, 0, 0]
            )
            .unwrap(),
            Return(6),
            "length query counts the terminator"
        );

        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "MultiByteToWideChar",
                &[1252, 0, source, u32::MAX, destination, 16]
            )
            .unwrap(),
            Return(6)
        );
        let mut wide = Vec::new();
        for index in 0..6 {
            wide.extend_from_slice(&memory.read_u16(destination + index * 2).unwrap().to_le_bytes());
        }
        assert_eq!(&wide[..10], &[b'V', 0, b'i', 0, 0xea, 0, 0xf2, 0, b't', 0]);

        // And back again.
        let back = STACK_BASE + 0x3200;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "WideCharToMultiByte",
                &[1252, 0, destination, u32::MAX, back, 16, 0, 0]
            )
            .unwrap(),
            Return(6)
        );
        assert_eq!(memory.read_bytes(back, 6).unwrap(), b"Vi\xea\xf2t\0");
    }

    #[test]
    fn startup_info_is_zeroed_with_its_size() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let info = STACK_BASE + 0x4000;
        memory.write_bytes(info, &[0xff; 68]).unwrap();
        call_shim(&mut host, &mut memory, "GetStartupInfoA", &[info]).unwrap();
        assert_eq!(memory.read_u32(info).unwrap(), 68);
        assert_eq!(memory.read_bytes(info + 4, 64).unwrap(), &[0_u8; 64]);
    }

    #[test]
    fn exit_process_asks_the_session_to_stop() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "ExitProcess", &[3]).unwrap(),
            Exit(3)
        );
    }

    #[test]
    fn a_raising_guest_is_reported_not_ignored() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let error = call_shim(&mut host, &mut memory, "RaiseException", &[0xc000_0005, 0, 0, 0])
            .unwrap_err();
        assert!(error.contains("0xc0000005"), "{error}");
    }
}
