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

use std::collections::HashMap;

use crate::objects::{Object, NO_OWNER, WAIT_OBJECT_0, WAIT_TIMEOUT};
use crate::pe::IMAGE_BASE;
use crate::sched::{Spawn, ThreadWant};
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::{Exit, Return};

const KERNEL32: &str = "KERNEL32.dll";

/// Windows XP, 5.1 build 2600. The game predates the browser, so claiming a
/// 2001-era platform keeps its version checks on the branch the art was drawn
/// for; the localization's own volume fix targets newer versions separately.
const WINDOWS_VERSION: u32 = 0x0a28_0105;

/// `GetLastError` after a handle could not be opened.
const ERROR_FILE_NOT_FOUND: u32 = 2;
/// `GetLastError` after `FindResourceA` found nothing of that type and name.
const ERROR_RESOURCE_NAME_NOT_FOUND: u32 = 1813;
/// `GetLastError` after a call was given a handle that is not open.
const ERROR_INVALID_HANDLE: u32 = 6;
/// `CreateThread` and `WaitForMultipleObjects` reject a null routine or a count
/// of zero with this rather than starting a thread that runs nowhere.
const ERROR_INVALID_PARAMETER: u32 = 87;

/// `INFINITE`: a wait with no deadline.
const INFINITE: u32 = u32::MAX;
/// `MAXIMUM_WAIT_OBJECTS`, the most handles one wait may name.
const MAXIMUM_WAIT_OBJECTS: u32 = 64;
/// `WAIT_FAILED`: the wait itself was refused, which is not the same as timing
/// out and is the only value a guest is expected to check for.
const WAIT_FAILED: u32 = u32::MAX;
/// The tick rate [`query_performance_counter`] reports: 1024 ticks per
/// millisecond of page clock.
const QPC_FREQUENCY: u32 = 1_024_000;

/// `GetFileSize`'s failure value.
const INVALID_FILE_SIZE: u32 = 0xffff_ffff;

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

/// The thread-local slots `TlsAlloc` hands out: `TlsSetValue` writes a value and
/// `TlsGetValue` reads it back, which is exactly what the CRT's
/// `__declspec(thread)` emulation and an SDL build's thread-local error state do
/// with it.
///
/// The indices belong to the session and the values to the thread: `TlsAlloc`
/// hands out one index every thread shares -- that is what makes a TLS variable
/// usable across threads at all -- while the value at that index is a thread's
/// own. The running thread's values are in [`ThreadLocals::slots`] and every
/// other thread's wait in `by_thread`, put there when the CPU leaves it.
pub struct ThreadLocals {
    slots: Vec<Option<u32>>,
    /// The values of the threads that are not running, by thread id.
    by_thread: HashMap<u32, Vec<Option<u32>>>,
    /// The thread whose values are in `slots`.
    current: u32,
}

impl Default for ThreadLocals {
    fn default() -> Self {
        Self {
            slots: Vec::new(),
            by_thread: HashMap::new(),
            current: 0,
        }
    }
}

impl ThreadLocals {
    /// Allocates a slot and returns its index. Windows grows its table the same
    /// way, one index at a time, and never reuses one until it is freed.
    ///
    /// Every thread's set grows, not just the running one's: an index means the
    /// same variable in each of them.
    pub fn alloc(&mut self) -> u32 {
        self.slots.push(None);
        for values in self.by_thread.values_mut() {
            values.push(None);
        }
        (self.slots.len() - 1) as u32
    }

    pub fn set(&mut self, index: u32, value: u32) {
        if let Some(slot) = self.slots.get_mut(index as usize) {
            *slot = Some(value);
        }
    }

    pub fn get(&self, index: u32) -> u32 {
        self.slots
            .get(index as usize)
            .copied()
            .flatten()
            .unwrap_or(0)
    }

    /// A freed slot reads as zero and is available to the next `TlsAlloc`.
    pub fn free(&mut self, index: u32) {
        if let Some(slot) = self.slots.get_mut(index as usize) {
            *slot = None;
        }
    }

    /// Hands the slots over to another thread: the running thread's values are
    /// put away and `thread`'s are picked up. A thread this has not seen starts
    /// with every slot zero, as a fresh thread does on Windows.
    pub fn switch_to(&mut self, thread: u32) {
        if self.current == thread {
            return;
        }
        let running = std::mem::take(&mut self.slots);
        self.by_thread.insert(self.current, running);
        self.current = thread;
        // The sets are kept the same length, so the slot count is whatever the
        // threads this host has already seen agreed on.
        let count = self.by_thread.values().map(Vec::len).max().unwrap_or(0);
        let mut values = self.by_thread.remove(&thread).unwrap_or_default();
        values.resize(count, None);
        self.slots = values;
    }
}

const LCMAP_LOWERCASE: u32 = 0x0000_0100;
const LCMAP_UPPERCASE: u32 = 0x0000_0200;
const NORM_IGNORECASE: u32 = 0x0000_0001;

pub const SHIMS: &[Binding] = &[
    Binding { dll: KERNEL32, name: "AreFileApisANSI", arg_bytes: 0, shim: are_file_apis_ansi },
    Binding { dll: KERNEL32, name: "Beep", arg_bytes: 8, shim: beep },
    Binding { dll: KERNEL32, name: "CloseHandle", arg_bytes: 4, shim: close_handle },
    Binding { dll: KERNEL32, name: "CreateFileMappingA", arg_bytes: 24, shim: create_file_mapping_a },
    Binding { dll: KERNEL32, name: "MapViewOfFile", arg_bytes: 20, shim: map_view_of_file },
    Binding { dll: KERNEL32, name: "UnmapViewOfFile", arg_bytes: 4, shim: unmap_view_of_file },
    Binding { dll: KERNEL32, name: "CompareStringA", arg_bytes: 24, shim: compare_string_a },
    Binding { dll: KERNEL32, name: "CompareStringW", arg_bytes: 24, shim: compare_string_w },
    Binding { dll: KERNEL32, name: "CreateDirectoryA", arg_bytes: 8, shim: create_directory },
    Binding { dll: KERNEL32, name: "CreateFileW", arg_bytes: 28, shim: create_file_w },
    Binding { dll: KERNEL32, name: "DecodePointer", arg_bytes: 4, shim: decode_pointer },
    Binding { dll: KERNEL32, name: "DeleteCriticalSection", arg_bytes: 4, shim: delete_critical_section },
    Binding { dll: KERNEL32, name: "EncodePointer", arg_bytes: 4, shim: encode_pointer },
    Binding { dll: KERNEL32, name: "EnterCriticalSection", arg_bytes: 4, shim: enter_critical_section },
    Binding { dll: KERNEL32, name: "ExitProcess", arg_bytes: 4, shim: exit_process },
    Binding { dll: KERNEL32, name: "FileTimeToLocalFileTime", arg_bytes: 8, shim: file_time_to_local_file_time },
    Binding { dll: KERNEL32, name: "FindResourceA", arg_bytes: 12, shim: find_resource },
    Binding { dll: KERNEL32, name: "LoadResource", arg_bytes: 8, shim: load_resource },
    Binding { dll: KERNEL32, name: "LockResource", arg_bytes: 4, shim: lock_resource },
    Binding { dll: KERNEL32, name: "SizeofResource", arg_bytes: 8, shim: size_of_resource },
    Binding { dll: KERNEL32, name: "FileTimeToSystemTime", arg_bytes: 8, shim: file_time_to_system_time },
    Binding { dll: KERNEL32, name: "FindNextFileA", arg_bytes: 8, shim: find_next_file },
    Binding { dll: KERNEL32, name: "FindNextFileW", arg_bytes: 8, shim: find_next_file_w },
    Binding { dll: KERNEL32, name: "GetCurrentDirectoryW", arg_bytes: 8, shim: get_current_directory_w },
    Binding { dll: KERNEL32, name: "GetUserDefaultLangID", arg_bytes: 0, shim: get_user_default_lang_id },
    Binding { dll: KERNEL32, name: "FreeEnvironmentStringsA", arg_bytes: 4, shim: free_environment_strings },
    Binding { dll: KERNEL32, name: "FreeEnvironmentStringsW", arg_bytes: 4, shim: free_environment_strings },
    Binding { dll: KERNEL32, name: "GetACP", arg_bytes: 0, shim: get_acp },
    Binding { dll: KERNEL32, name: "GetCPInfo", arg_bytes: 8, shim: get_cp_info },
    Binding { dll: KERNEL32, name: "GetCommandLineA", arg_bytes: 0, shim: get_command_line },
    Binding { dll: KERNEL32, name: "GetCurrentProcess", arg_bytes: 0, shim: get_current_process },
    Binding { dll: KERNEL32, name: "GetCurrentProcessId", arg_bytes: 0, shim: get_current_process_id },
    Binding { dll: KERNEL32, name: "GetCurrentThreadId", arg_bytes: 0, shim: get_current_thread_id },
    Binding { dll: KERNEL32, name: "GetDateFormatW", arg_bytes: 24, shim: get_date_format_w },
    Binding { dll: KERNEL32, name: "GetEnvironmentStrings", arg_bytes: 0, shim: get_environment_strings },
    Binding { dll: KERNEL32, name: "GetEnvironmentStringsW", arg_bytes: 0, shim: get_environment_strings_w },
    Binding { dll: KERNEL32, name: "GetFileAttributesA", arg_bytes: 4, shim: get_file_attributes },
    Binding { dll: KERNEL32, name: "GetFileSize", arg_bytes: 8, shim: get_file_size },
    Binding { dll: KERNEL32, name: "GetFileType", arg_bytes: 4, shim: get_file_type },
    Binding { dll: KERNEL32, name: "GetLastError", arg_bytes: 0, shim: get_last_error },
    Binding { dll: KERNEL32, name: "GetLocalTime", arg_bytes: 4, shim: get_local_time },
    Binding { dll: KERNEL32, name: "GetLogicalDrives", arg_bytes: 0, shim: get_logical_drives },
    Binding { dll: KERNEL32, name: "GetModuleFileNameA", arg_bytes: 12, shim: get_module_file_name },
    Binding { dll: KERNEL32, name: "GetModuleFileNameW", arg_bytes: 12, shim: get_module_file_name_w },
    Binding { dll: KERNEL32, name: "GetModuleHandleA", arg_bytes: 4, shim: get_module_handle },
    Binding { dll: KERNEL32, name: "GetModuleHandleExW", arg_bytes: 12, shim: get_module_handle_ex },
    Binding { dll: KERNEL32, name: "GetModuleHandleW", arg_bytes: 4, shim: get_module_handle_w },
    Binding { dll: KERNEL32, name: "GetOEMCP", arg_bytes: 0, shim: get_oem_cp },
    Binding { dll: KERNEL32, name: "GetProfileStringA", arg_bytes: 20, shim: get_profile_string },
    Binding { dll: KERNEL32, name: "GetProcAddress", arg_bytes: 8, shim: get_proc_address },
    Binding { dll: KERNEL32, name: "GetProcessHeap", arg_bytes: 0, shim: get_process_heap },
    Binding { dll: KERNEL32, name: "GetStartupInfoA", arg_bytes: 4, shim: get_startup_info },
    Binding { dll: KERNEL32, name: "GetStartupInfoW", arg_bytes: 4, shim: get_startup_info_w },
    Binding { dll: KERNEL32, name: "GetStdHandle", arg_bytes: 4, shim: get_std_handle },
    Binding { dll: KERNEL32, name: "GetStringTypeA", arg_bytes: 20, shim: get_string_type_a },
    Binding { dll: KERNEL32, name: "GetStringTypeW", arg_bytes: 16, shim: get_string_type_w },
    Binding { dll: KERNEL32, name: "GetSystemTime", arg_bytes: 4, shim: get_system_time },
    Binding { dll: KERNEL32, name: "GetSystemTimeAsFileTime", arg_bytes: 4, shim: get_system_time_as_file_time },
    Binding { dll: KERNEL32, name: "GetTimeFormatW", arg_bytes: 24, shim: get_time_format_w },
    Binding { dll: KERNEL32, name: "GetTimeZoneInformation", arg_bytes: 4, shim: get_time_zone_information },
    Binding { dll: KERNEL32, name: "GetVersion", arg_bytes: 0, shim: get_version },
    Binding { dll: KERNEL32, name: "GlobalLock", arg_bytes: 4, shim: global_lock },
    Binding { dll: KERNEL32, name: "GlobalUnlock", arg_bytes: 4, shim: global_unlock },
    Binding { dll: KERNEL32, name: "HeapAlloc", arg_bytes: 12, shim: heap_alloc },
    Binding { dll: KERNEL32, name: "HeapCreate", arg_bytes: 12, shim: heap_create },
    Binding { dll: KERNEL32, name: "HeapDestroy", arg_bytes: 4, shim: heap_destroy },
    Binding { dll: KERNEL32, name: "HeapFree", arg_bytes: 12, shim: heap_free },
    Binding { dll: KERNEL32, name: "HeapReAlloc", arg_bytes: 16, shim: heap_realloc },
    Binding { dll: KERNEL32, name: "HeapSize", arg_bytes: 12, shim: heap_size },
    Binding { dll: KERNEL32, name: "InitializeCriticalSection", arg_bytes: 4, shim: initialize_critical_section },
    Binding { dll: KERNEL32, name: "InitializeCriticalSectionAndSpinCount", arg_bytes: 8, shim: initialize_critical_section },
    Binding { dll: KERNEL32, name: "InterlockedDecrement", arg_bytes: 4, shim: interlocked_decrement },
    Binding { dll: KERNEL32, name: "InterlockedIncrement", arg_bytes: 4, shim: interlocked_increment },
    Binding { dll: KERNEL32, name: "IsBadCodePtr", arg_bytes: 4, shim: is_bad_pointer },
    Binding { dll: KERNEL32, name: "IsBadReadPtr", arg_bytes: 8, shim: is_bad_pointer },
    Binding { dll: KERNEL32, name: "IsBadWritePtr", arg_bytes: 8, shim: is_bad_pointer },
    Binding { dll: KERNEL32, name: "IsDebuggerPresent", arg_bytes: 0, shim: is_debugger_present },
    Binding { dll: KERNEL32, name: "IsProcessorFeaturePresent", arg_bytes: 4, shim: is_processor_feature_present },
    Binding { dll: KERNEL32, name: "IsValidCodePage", arg_bytes: 4, shim: is_valid_code_page },
    Binding { dll: KERNEL32, name: "LCMapStringA", arg_bytes: 24, shim: lc_map_string_a },
    Binding { dll: KERNEL32, name: "LCMapStringW", arg_bytes: 24, shim: lc_map_string_w },
    Binding { dll: KERNEL32, name: "LeaveCriticalSection", arg_bytes: 4, shim: leave_critical_section },
    Binding { dll: KERNEL32, name: "LoadLibraryA", arg_bytes: 4, shim: load_library },
    Binding { dll: KERNEL32, name: "LoadLibraryExW", arg_bytes: 12, shim: load_library_w },
    Binding { dll: KERNEL32, name: "LoadLibraryW", arg_bytes: 4, shim: load_library_w },
    Binding { dll: KERNEL32, name: "MultiByteToWideChar", arg_bytes: 24, shim: multi_byte_to_wide },
    Binding { dll: KERNEL32, name: "QueryPerformanceCounter", arg_bytes: 4, shim: query_performance_counter },
    Binding { dll: KERNEL32, name: "QueryPerformanceFrequency", arg_bytes: 4, shim: query_performance_frequency },
    Binding { dll: KERNEL32, name: "RaiseException", arg_bytes: 16, shim: raise_exception },
    Binding { dll: KERNEL32, name: "CreateEventA", arg_bytes: 16, shim: create_event },
    Binding { dll: KERNEL32, name: "CreateMutexA", arg_bytes: 12, shim: create_mutex },
    Binding { dll: KERNEL32, name: "OpenMutexA", arg_bytes: 12, shim: open_mutex },
    Binding { dll: KERNEL32, name: "ReleaseMutex", arg_bytes: 4, shim: release_mutex },
    Binding { dll: KERNEL32, name: "CreateThread", arg_bytes: 24, shim: create_thread },
    Binding { dll: KERNEL32, name: "ExitThread", arg_bytes: 4, shim: exit_thread },
    Binding { dll: KERNEL32, name: "GetConsoleCP", arg_bytes: 0, shim: get_console_cp },
    Binding { dll: KERNEL32, name: "GetCurrentDirectoryA", arg_bytes: 8, shim: get_current_directory },
    Binding { dll: KERNEL32, name: "GetTickCount", arg_bytes: 0, shim: get_tick_count },
    Binding { dll: KERNEL32, name: "GetThreadLocale", arg_bytes: 0, shim: get_thread_locale },
    Binding { dll: KERNEL32, name: "GetVersionExA", arg_bytes: 4, shim: get_version_ex },
    Binding { dll: KERNEL32, name: "GlobalAlloc", arg_bytes: 8, shim: global_alloc },
    Binding { dll: KERNEL32, name: "LocalAlloc", arg_bytes: 8, shim: local_alloc },
    Binding { dll: KERNEL32, name: "LocalFree", arg_bytes: 4, shim: local_free },
    Binding { dll: KERNEL32, name: "LocalLock", arg_bytes: 4, shim: local_lock },
    Binding { dll: KERNEL32, name: "LocalSize", arg_bytes: 4, shim: local_size },
    Binding { dll: KERNEL32, name: "LocalUnlock", arg_bytes: 4, shim: local_unlock },
    Binding { dll: KERNEL32, name: "GlobalSize", arg_bytes: 4, shim: global_size },
    Binding { dll: KERNEL32, name: "SetCurrentDirectoryA", arg_bytes: 4, shim: set_current_directory },
    Binding { dll: KERNEL32, name: "SetEvent", arg_bytes: 4, shim: set_event },
    Binding { dll: KERNEL32, name: "WaitForMultipleObjects", arg_bytes: 16, shim: wait_for_multiple_objects },
    Binding { dll: KERNEL32, name: "WaitForSingleObject", arg_bytes: 8, shim: wait_for_single_object },
    Binding { dll: KERNEL32, name: "lstrlenA", arg_bytes: 4, shim: lstrlen },
    Binding { dll: KERNEL32, name: "GetConsoleMode", arg_bytes: 8, shim: get_console_mode },
    Binding { dll: KERNEL32, name: "OutputDebugStringW", arg_bytes: 4, shim: output_debug_string_w },
    Binding { dll: KERNEL32, name: "ReadConsoleW", arg_bytes: 20, shim: read_console_w },
    Binding { dll: KERNEL32, name: "RtlUnwind", arg_bytes: 16, shim: rtl_unwind },
    Binding { dll: KERNEL32, name: "SetEnvironmentVariableA", arg_bytes: 8, shim: set_environment_variable },
    Binding { dll: KERNEL32, name: "SetFilePointerEx", arg_bytes: 20, shim: set_file_pointer_ex },
    Binding { dll: KERNEL32, name: "SetHandleCount", arg_bytes: 4, shim: set_handle_count },
    Binding { dll: KERNEL32, name: "SetLastError", arg_bytes: 4, shim: set_last_error },
    Binding { dll: KERNEL32, name: "SetStdHandle", arg_bytes: 8, shim: set_std_handle },
    Binding { dll: KERNEL32, name: "SetUnhandledExceptionFilter", arg_bytes: 4, shim: set_unhandled_exception_filter },
    Binding { dll: KERNEL32, name: "Sleep", arg_bytes: 4, shim: sleep },
    Binding { dll: KERNEL32, name: "TerminateProcess", arg_bytes: 8, shim: terminate_process },
    Binding { dll: KERNEL32, name: "TlsAlloc", arg_bytes: 0, shim: tls_alloc },
    Binding { dll: KERNEL32, name: "TlsFree", arg_bytes: 4, shim: tls_free },
    Binding { dll: KERNEL32, name: "TlsGetValue", arg_bytes: 4, shim: tls_get_value },
    Binding { dll: KERNEL32, name: "TlsSetValue", arg_bytes: 8, shim: tls_set_value },
    Binding { dll: KERNEL32, name: "UnhandledExceptionFilter", arg_bytes: 4, shim: unhandled_exception_filter },
    Binding { dll: KERNEL32, name: "VirtualAlloc", arg_bytes: 16, shim: virtual_alloc },
    Binding { dll: KERNEL32, name: "VirtualFree", arg_bytes: 12, shim: virtual_free },
    Binding { dll: KERNEL32, name: "WideCharToMultiByte", arg_bytes: 32, shim: wide_to_multi_byte },
    Binding { dll: KERNEL32, name: "WriteConsoleW", arg_bytes: 20, shim: write_console_w },
    Binding { dll: KERNEL32, name: "lstrcmpiA", arg_bytes: 8, shim: lstrcmpi },
    Binding { dll: KERNEL32, name: "lstrcpyA", arg_bytes: 8, shim: lstrcpy },
    Binding { dll: KERNEL32, name: "lstrcpynA", arg_bytes: 12, shim: lstrcpyn },
    Binding { dll: KERNEL32, name: "FindResourceW", arg_bytes: 12, shim: find_resource_w },
    Binding { dll: KERNEL32, name: "GetLocaleInfoA", arg_bytes: 16, shim: get_locale_info },
    Binding { dll: KERNEL32, name: "GetLocaleInfoW", arg_bytes: 16, shim: get_locale_info_w },
    Binding { dll: KERNEL32, name: "InterlockedCompareExchange", arg_bytes: 12, shim: interlocked_compare_exchange },
    Binding { dll: KERNEL32, name: "InterlockedExchange", arg_bytes: 8, shim: interlocked_exchange },
    Binding { dll: KERNEL32, name: "OutputDebugStringA", arg_bytes: 4, shim: output_debug_string },
    // The start-up path a 2005 Direct3D 8 game takes: a worker semaphore it never
    // waits on, the sizing and drive questions an installer asks, and the two
    // memory calls its allocator makes before the device exists.
    Binding { dll: KERNEL32, name: "CancelIo", arg_bytes: 4, shim: succeed },
    Binding { dll: KERNEL32, name: "CreateSemaphoreA", arg_bytes: 16, shim: create_semaphore },
    Binding { dll: KERNEL32, name: "FreeLibrary", arg_bytes: 4, shim: succeed },
    Binding { dll: KERNEL32, name: "GetDiskFreeSpaceExA", arg_bytes: 16, shim: get_disk_free_space_ex },
    Binding { dll: KERNEL32, name: "GetDriveTypeA", arg_bytes: 4, shim: get_drive_type },
    Binding { dll: KERNEL32, name: "GetFileAttributesExA", arg_bytes: 12, shim: get_file_attributes_ex },
    Binding { dll: KERNEL32, name: "GetOverlappedResult", arg_bytes: 16, shim: get_overlapped_result },
    Binding { dll: KERNEL32, name: "GetSystemDirectoryA", arg_bytes: 8, shim: get_system_directory },
    Binding { dll: KERNEL32, name: "GetSystemInfo", arg_bytes: 4, shim: get_system_info },
    Binding { dll: KERNEL32, name: "ReleaseSemaphore", arg_bytes: 12, shim: release_semaphore },
    Binding { dll: KERNEL32, name: "ResumeThread", arg_bytes: 4, shim: resume_thread },
    Binding { dll: KERNEL32, name: "SetThreadPriority", arg_bytes: 8, shim: succeed },
    Binding { dll: KERNEL32, name: "VirtualProtect", arg_bytes: 16, shim: virtual_protect },
    Binding { dll: KERNEL32, name: "VirtualQuery", arg_bytes: 12, shim: virtual_query },
];

// ---- process and version -------------------------------------------------

fn get_version(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(WINDOWS_VERSION))
}

/// `GetVersionExA(info)`: the same Windows XP `GetVersion` answers, written into
/// an `OSVERSIONINFOA`. The guest states how much it allocated in the first
/// field, and a buffer too small for a field is left alone.
fn get_version_ex(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const CSD_BYTES: u32 = 128;
    let info = call.arg(0)?;
    if info == 0 {
        return Ok(Return(0));
    }
    let size = call.memory.read_u32(info)?;
    let fields = [
        (4, 5),    // dwMajorVersion
        (8, 1),    // dwMinorVersion
        (12, 2600), // dwBuildNumber
        (16, 2),   // dwPlatformId: VER_PLATFORM_WIN32_NT
    ];
    for (offset, value) in fields {
        if size >= offset + 4 {
            call.write_u32(info + offset, value)?;
        }
    }
    if size > 20 {
        call.zero(info + 20, (size - 20).min(CSD_BYTES))?;
    }
    Ok(Return(1))
}

/// `LocalAlloc(flags, bytes)`: the same arena `HeapAlloc` and `GlobalAlloc` hand
/// out, so a pointer from one is a pointer for the others.
fn local_alloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let size = call.arg(1)?;
    match host.allocate(size, 8) {
        Ok(pointer) => Ok(Return(pointer)),
        Err(error) => Ok(Return(refuse(host, call, "LocalAlloc", size, error))),
    }
}

fn local_free(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn local_size(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.allocated(call.arg(0)?).unwrap_or(0)))
}

fn local_lock(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?))
}

/// An import whose whole answer is "done": the handle it was given is a number
/// with no resource behind it, so there is nothing to do and nothing to refuse.
fn succeed(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `CreateSemaphoreA(attributes, initial, maximum, name)`: a count, which a
/// wait lowers and `ReleaseSemaphore` raises. A worker that waits for a job
/// hangs off exactly this.
fn create_semaphore(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let count = call.arg(1)?;
    let maximum = call.arg(2)?;
    let handle = host.objects.insert(Object::Semaphore { count, maximum });
    if std::env::var("SHIMKIT_LOG_OBJECTS").is_ok() {
        println!("CreateSemaphoreA initial={count} max={maximum} -> {handle}");
    }
    Ok(Return(handle))
}

/// `ReleaseSemaphore(handle, count, previous)`: raises the count, and hands it
/// to whoever has been waiting on it longest.
fn release_semaphore(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let count = call.arg(1)?;
    let previous = call.arg(2)?;
    let before = match host.objects.get(handle) {
        Some(Object::Semaphore { count, .. }) => count,
        // Windows itself fails this call on anything that is not a semaphore, but
        // the guest's own bookkeeping handles are not this host's to judge: the
        // count is raised where there is one and the call succeeds.
        _ => 0,
    };
    if previous != 0 {
        call.write_u32(previous, before)?;
    }
    if std::env::var("SHIMKIT_LOG_OBJECTS").is_ok() {
        println!("ReleaseSemaphore {handle} +{count} (was {before})");
    }
    host.objects.raise(handle, count);
    host.woken.extend(host.objects.wake(handle));
    Ok(Return(1))
}

/// `ResumeThread` and `SuspendThread`: a thread this host starts runs from the
/// moment it is created, so the previous suspend count was the only one.
fn resume_thread(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `GetDriveTypeA(root)`: the payload is a folder on a fixed disk, which is the
/// answer that keeps an installer's "where should this go" path on its normal
/// branch.
fn get_drive_type(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    const DRIVE_FIXED: u32 = 3;
    Ok(Return(DRIVE_FIXED))
}

/// `GetDiskFreeSpaceExA(dir, &available, &total, &free)`: a gigabyte free.
///
/// The real number would be the browser's storage quota, which no guest can
/// query; a game that checks for room to save decides on this alone.
fn get_disk_free_space_ex(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const GIB: u64 = 1024 * 1024 * 1024;
    for index in 1..=3 {
        let at = call.arg(index)?;
        if at != 0 {
            call.write_u32(at, GIB as u32)?;
            call.write_u32(at.wrapping_add(4), (GIB >> 32) as u32)?;
        }
    }
    Ok(Return(1))
}

fn get_system_directory(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const DIRECTORY: &str = "C:\\WINDOWS\\system32";
    let buffer = call.arg(0)?;
    let capacity = call.arg(1)? as usize;
    if buffer == 0 || capacity == 0 {
        return Ok(Return(0));
    }
    let written = (capacity - 1).min(DIRECTORY.len());
    call.write(buffer, &DIRECTORY.as_bytes()[..written])?;
    call.write(buffer.wrapping_add(written as u32), &[0])?;
    Ok(Return(written as u32))
}

/// `GetFileAttributesExA(name, level, &data)`: the file table's answer as a
/// `WIN32_FILE_ATTRIBUTE_DATA`.
///
/// The times are left zero, because a payload's files have no dates of their own
/// until the browser reports one; the size is what a guest actually reads from
/// here, and a name the table does not hold is the call's own failure.
fn get_file_attributes_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const FILE_ATTRIBUTE_NORMAL: u32 = 0x80;
    const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;
    let path = call.cstr(0)?;
    let data = call.arg(2)?;
    if data == 0 {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(Return(0));
    }
    // A name that names a folder is one a saved path ends in.
    let directory = path.ends_with('\\') || host.files.exists(&format!("{path}\\"));
    let Some(length) = host.files.contents(&path).map(|bytes| bytes.len() as u32) else {
        if !directory {
            host.last_error = ERROR_FILE_NOT_FOUND;
            return Ok(Return(0));
        }
        let attributes = FILE_ATTRIBUTE_DIRECTORY;
        call.zero(data, 36)?;
        call.write_u32(data, attributes)?;
        return Ok(Return(1));
    };
    call.zero(data, 36)?;
    call.write_u32(data, FILE_ATTRIBUTE_NORMAL)?;
    call.write_u32(data.wrapping_add(28), 0)?;
    call.write_u32(data.wrapping_add(32), length)?;
    Ok(Return(1))
}

/// `GetOverlappedResult(handle, &overlapped, &transferred, wait)`: every read
/// this host performs finishes before it returns, so the result is already in
/// the structure and the call reports the byte count the transfer wrote there.
fn get_overlapped_result(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let overlapped = call.arg(1)?;
    let transferred = call.arg(2)?;
    let count = if overlapped != 0 {
        call.memory.read_u32(overlapped.wrapping_add(4))?
    } else {
        0
    };
    if transferred != 0 {
        call.write_u32(transferred, count)?;
    }
    Ok(Return(1))
}

/// `GetSystemInfo(&info)`: one Intel processor.
///
/// A game asks this to size its worker threads and to find the address range it
/// may allocate in; both answers are this host's real ones.
fn get_system_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const PROCESSOR_INTEL_PENTIUM: u32 = 586;
    let info = call.arg(0)?;
    if info == 0 {
        return Ok(Return(0));
    }
    call.zero(info, 36)?;
    // wProcessorArchitecture stays zero: PROCESSOR_ARCHITECTURE_INTEL.
    call.write_u32(info.wrapping_add(4), 4096)?; // dwPageSize
    call.write_u32(info.wrapping_add(8), 0x0001_0000)?; // lpMinimumApplicationAddress
    call.write_u32(info.wrapping_add(12), crate::memory::MEMORY_TOP)?;
    call.write_u32(info.wrapping_add(16), 1)?; // dwActiveProcessorMask
    call.write_u32(info.wrapping_add(20), 1)?; // dwNumberOfProcessors
    call.write_u32(info.wrapping_add(24), PROCESSOR_INTEL_PENTIUM)?;
    call.write_u32(info.wrapping_add(28), 0x0001_0000)?; // dwAllocationGranularity
    call.write_u16(info.wrapping_add(32), 6)?; // wProcessorLevel
    call.write_u16(info.wrapping_add(34), 0x0a00)?; // wProcessorRevision
    let _ = host;
    Ok(Return(1))
}

/// `VirtualProtect(address, size, new, &old)`: the guest's own allocation is
/// already writable, and the old protection it is told is the one it had.
fn virtual_protect(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const PAGE_READWRITE: u32 = 0x04;
    let previous = call.arg(3)?;
    if previous != 0 {
        call.write_u32(previous, PAGE_READWRITE)?;
    }
    Ok(Return(1))
}

/// `VirtualQuery(address, &info, length)`: one committed private page range
/// covering the address asked about.
///
/// A guest uses this to walk what is mapped before it decides where to put an
/// allocation; the shape of the answer matters more than its size.
fn virtual_query(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const MEM_COMMIT: u32 = 0x1000;
    const MEM_PRIVATE: u32 = 0x2_0000;
    const PAGE_READWRITE: u32 = 0x04;
    const INFO_BYTES: u32 = 28;
    let address = call.arg(0)?;
    let info = call.arg(1)?;
    let length = call.arg(2)?;
    if info == 0 || length < INFO_BYTES {
        return Ok(Return(0));
    }
    let base = address & !0xfff;
    call.zero(info, INFO_BYTES)?;
    for (offset, value) in [
        (0, base),               // BaseAddress
        (4, base),               // AllocationBase
        (8, PAGE_READWRITE),     // AllocationProtect
        (12, 0x10_0000),         // RegionSize
        (16, MEM_COMMIT),        // State
        (20, PAGE_READWRITE),    // Protect
        (24, MEM_PRIVATE),       // Type
    ] {
        call.write_u32(info.wrapping_add(offset), value)?;
    }
    Ok(Return(INFO_BYTES))
}

fn local_unlock(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

// ---- shared state, locale and the debugger -------------------------------

/// `InterlockedExchange(target, value)`: the previous value, with the write in
/// the same step.
///
/// One instruction stream means no other thread can be between the read and the
/// write, which is exactly the guarantee the guest is asking for.
fn interlocked_exchange(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let target = call.arg(0)?;
    let value = call.arg(1)?;
    if target == 0 {
        return Ok(Return(0));
    }
    let previous = call.memory.read_u32(target)?;
    call.write_u32(target, value)?;
    Ok(Return(previous))
}

/// `InterlockedCompareExchange(target, exchange, comparand)`: the previous value,
/// which is also how the guest learns whether the exchange happened.
fn interlocked_compare_exchange(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let target = call.arg(0)?;
    let exchange = call.arg(1)?;
    let comparand = call.arg(2)?;
    if target == 0 {
        return Ok(Return(0));
    }
    let previous = call.memory.read_u32(target)?;
    if previous == comparand {
        call.write_u32(target, exchange)?;
    }
    Ok(Return(previous))
}

/// `OutputDebugStringA/W`: there is no debugger attached to read it.
///
/// The call is not a stop, because a program that logs is a program that works:
/// Windows itself discards the text when nothing is listening.
fn output_debug_string(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // `cstr` takes an argument *index*; this import's whole argument is the
    // address, and passing it as an index asks for argument 4497164 -- which is
    // how the first debug print of a logging program used to stop the guest.
    let text = call.memory.read_cstr(call.arg(0)?)?;
    if !text.is_empty() {
        let clipped: String = text.chars().take(200).collect();
        host.notice(format!("output: {clipped}"));
    }
    Ok(Return(0))
}

/// `GetLocaleInfoA(locale, kind, buffer, size)` and its wide twin.
///
/// This machine has one locale, English (United States), which is what
/// `GetThreadLocale` and `GetACP` already answer with. A field this table does
/// not know is a failure, as on Windows -- guessing a separator would put a
/// wrong number into a settings screen.
fn get_locale_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = match locale_text(call.arg(1)?) {
        Some(text) => text,
        None => return unknown_locale_field(host),
    };
    write_locale_text(call, 2, 3, false, text)
}

fn get_locale_info_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = match locale_text(call.arg(1)?) {
        Some(text) => text,
        None => return unknown_locale_field(host),
    };
    write_locale_text(call, 2, 3, true, text)
}

fn unknown_locale_field(host: &mut Host) -> Result<Action> {
    host.last_error = 87; // ERROR_INVALID_PARAMETER
    Ok(Return(0))
}

/// Copies a locale field into the caller's buffer at `buffer` with `size` units
/// available: bytes when `wide` is false, UTF-16 units when it is.
///
/// The count includes the terminator, and a null buffer with a zero size asks
/// for that count alone.
fn write_locale_text(
    call: &mut Call<'_>,
    buffer_index: u32,
    size_index: u32,
    wide: bool,
    text: &str,
) -> Result<Action> {
    let buffer = call.arg(buffer_index)?;
    let size = call.arg(size_index)?;
    let units: Vec<u16> = text.encode_utf16().collect();
    let needed = units.len() + 1;
    if buffer == 0 || size == 0 {
        return Ok(Return(needed as u32));
    }
    if wide {
        let written = needed.min(size as usize);
        for (index, unit) in units.iter().take(written.saturating_sub(1)).enumerate() {
            call.write_u16(buffer + index as u32 * 2, *unit)?;
        }
        call.write_u16(buffer + (written as u32 - 1) * 2, 0)?;
        return Ok(Return(written as u32));
    }
    let written = needed.min(size as usize);
    let mut bytes = Vec::with_capacity(written);
    for unit in units.iter().take(written.saturating_sub(1)) {
        // The ANSI codepage is Latin-1 here: one byte per character it knows,
        // and `?` for anything else.
        bytes.push(if u32::from(*unit) < 0x100 { *unit as u8 } else { b'?' });
    }
    bytes.push(0);
    call.write(buffer, &bytes)?;
    Ok(Return(bytes.len() as u32))
}

/// The `LOCALE_*` fields a settings screen reads, for English (United States).
fn locale_text(kind: u32) -> Option<&'static str> {
    const DAYS: [&str; 7] = [
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
        "Sunday",
    ];
    const SHORT_DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];
    const SHORT_MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    Some(match kind {
        0x0000_0001 => "0409",                   // LOCALE_ILANGUAGE
        0x0000_000e => ".",                      // LOCALE_SDECIMAL
        0x0000_000f => ",",                      // LOCALE_STHOUSAND
        0x0000_0010 => "3;0",                    // LOCALE_SGROUPING
        0x0000_0011 => "2",                      // LOCALE_IDIGITS
        0x0000_0012 => "1",                      // LOCALE_ILZERO
        0x0000_0014 => "0123456789",             // LOCALE_SNATIVEDIGITS
        0x0000_001d => "/",                      // LOCALE_SDATE
        0x0000_001e => ":",                      // LOCALE_STIME
        0x0000_001f => "M/d/yyyy",               // LOCALE_SSHORTDATE
        0x0000_0020 => "dddd, MMMM d, yyyy",      // LOCALE_SLONGDATE
        0x0000_0023 => "0",                      // LOCALE_ITIME (12-hour)
        0x0000_0028 => "AM",                     // LOCALE_S1159
        0x0000_0029 => "PM",                     // LOCALE_S2359
        0x0000_002a..=0x0000_0030 => DAYS[(kind - 0x2a) as usize],
        0x0000_0031..=0x0000_0037 => SHORT_DAYS[(kind - 0x31) as usize],
        0x0000_0038..=0x0000_0043 => MONTHS[(kind - 0x38) as usize],
        0x0000_0044..=0x0000_004f => SHORT_MONTHS[(kind - 0x44) as usize],
        0x0000_0059 => "en",                     // LOCALE_SISO639LANGNAME
        0x0000_005a => "US",                     // LOCALE_SISO3166CTRYNAME
        0x0000_005c => "en-US",                  // LOCALE_SNAME
        0x0000_1003 => "h:mm:ss tt",             // LOCALE_STIMEFORMAT
        0x0000_1004 => "dddd, MMMM d, yyyy",      // LOCALE_SLOCALIZEDLONGDATE
        _ => return None,
    })
}

// ---- resources -----------------------------------------------------------

/// Handles for resources start here, above every other number this host makes.
const RESOURCE_HANDLE_BASE: u32 = 0x0001_0700;

/// `FindResourceA(module, name, type)`: an index into the image's own tree.
///
/// Both keys may be a string or a `MAKEINTRESOURCE` number -- that is, any value
/// below `0x10000` is an integer id rather than an address -- and the tree is
/// read once when the image loads, so this is a comparison and no parsing.
fn find_resource(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = resource_key(call, 1)?;
    let kind = resource_key(call, 2)?;
    find_with(host, name, kind)
}

/// `FindResourceW(module, name, type)`: the same tree, with wide names.
///
/// A `MAKEINTRESOURCEW` id is the same small integer, so only a string key
/// differs: it is UTF-16 at the address the caller passed.
fn find_resource_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = resource_key_w(call, 1)?;
    let kind = resource_key_w(call, 2)?;
    find_with(host, name, kind)
}

fn find_with(
    host: &mut Host,
    name: crate::pe::ResourceKey,
    kind: crate::pe::ResourceKey,
) -> Result<Action> {
    // Names compare the way Windows compares them, which is without case, so a
    // guest that names a resource in its own spelling still finds it.
    let found = host
        .resources
        .iter()
        .position(|resource| resource.kind.same(&kind) && resource.name.same(&name));
    if std::env::var("SHIMKIT_LOG_RESOURCES").is_ok() {
        // Which lookups a guest makes and which of them this host can answer is
        // what separates "the game skipped it" from "the resource is missing".
        println!("resource {kind:?}/{name:?} -> {}", found.is_some());
    }
    let Some(index) = found else {
        // A resource the image does not carry is the answer a game handles by
        // skipping it, so it is a return value and not a stop.
        host.last_error = ERROR_RESOURCE_NAME_NOT_FOUND;
        return Ok(Return(0));
    };
    Ok(Return(RESOURCE_HANDLE_BASE + index as u32))
}

/// `LoadResource(module, handle)`: the address of the resource's bytes.
///
/// On Windows this is an `HGLOBAL` that `LockResource` turns into a pointer. Here
/// it is the address itself: the bytes are already mapped, being part of the
/// image's `.rsrc`, and nothing is copied or refcounted.
fn load_resource(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(index) = resource_index(call.arg(1)?) else {
        return Ok(Return(0));
    };
    match host.resources.get(index) {
        Some(resource) => Ok(Return(crate::pe::IMAGE_BASE.wrapping_add(resource.rva))),
        None => Ok(Return(0)),
    }
}

/// `LockResource(handle)`: the loaded resource is already a pointer.
fn lock_resource(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?))
}

/// `SizeofResource(module, handle)`.
fn size_of_resource(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let Some(index) = resource_index(call.arg(1)?) else {
        return Ok(Return(0));
    };
    Ok(Return(
        host.resources.get(index).map_or(0, |resource| resource.size),
    ))
}

fn resource_index(handle: u32) -> Option<usize> {
    let index = handle.checked_sub(RESOURCE_HANDLE_BASE)?;
    usize::try_from(index).ok()
}

/// Reads a resource key argument: an integer id or a string address.
fn resource_key(call: &mut Call<'_>, index: u32) -> Result<crate::pe::ResourceKey> {
    let value = call.arg(index)?;
    if value < 0x1_0000 {
        return Ok(crate::pe::ResourceKey::Id(value));
    }
    Ok(crate::pe::ResourceKey::Name(call.memory.read_cstr(value)?))
}

/// The wide form: a `MAKEINTRESOURCEW` id, or UTF-16 text.
fn resource_key_w(call: &Call<'_>, index: u32) -> Result<crate::pe::ResourceKey> {
    let value = call.arg(index)?;
    if value < 0x1_0000 {
        return Ok(crate::pe::ResourceKey::Id(value));
    }
    Ok(crate::pe::ResourceKey::Name(read_wide(call, value, u32::MAX)?))
}

/// `GetThreadLocale`: English (United States). The guest uses it to pick a
/// resource language, and this machine has one language.
fn get_thread_locale(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0x0409))
}

fn get_last_error(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.last_error))
}

fn get_current_process(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(INVALID_HANDLE_VALUE))
}

fn get_current_process_id(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // One process, one thread: the CRT only needs the numbers to exist and to
    // stay the same for the life of the session.
    Ok(Return(1))
}

/// `GetCurrentThreadId`: the id of the thread the CPU is on, which is the one
/// `CreateThread` wrote out of its `threadId` argument.
fn get_current_thread_id(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.current_thread))
}

fn is_debugger_present(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn is_processor_feature_present(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // PF_* questions about SSE and friends: the interpreter implements x86, not
    // a CPUID leaf, so the answer is "no" and the CRT takes its fallback path.
    let _ = call.arg(0)?;
    Ok(Return(0))
}

fn encode_pointer(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Nothing here is a security boundary, so a cookie would only be a value the
    // guest has to remember. The pointer is its own encoding.
    Ok(Return(call.arg(0)?))
}

fn decode_pointer(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?))
}

fn set_last_error(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.last_error = call.arg(0)?;
    Ok(Return(0))
}

fn query_performance_counter(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    if at != 0 {
        // The page's clock in milliseconds, scaled the way a 1 GHz counter would
        // be: the guest only ever subtracts two readings.
        let ticks = host.now_ms().wrapping_mul(1024);
        call.write_u32(at, ticks as u32)?;
        call.write_u32(at.wrapping_add(4), (ticks >> 32) as u32)?;
    }
    Ok(Return(1))
}

fn query_performance_frequency(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    // The counter is the page's clock in milliseconds scaled by 1024, so one
    // second is 1024 readings per millisecond. A guest divides one by the other
    // and gets the seconds it is after.
    if at != 0 {
        call.write_u32(at, QPC_FREQUENCY)?;
        call.write_u32(at.wrapping_add(4), 0)?;
    }
    Ok(Return(1))
}

fn interlocked_increment(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    let value = call.memory.read_u32(at)?.wrapping_add(1);
    call.write_u32(at, value)?;
    Ok(Return(value))
}

fn interlocked_decrement(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    let value = call.memory.read_u32(at)?.wrapping_sub(1);
    call.write_u32(at, value)?;
    Ok(Return(value))
}

/// `CreateEventA(attributes, manualReset, initialState, name)`: a flag that a
/// wait can clear and `SetEvent` can raise again.
fn create_event(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let manual = call.arg(1)? != 0;
    let signalled = call.arg(2)? != 0;
    Ok(Return(host.objects.insert(Object::Event { manual, signalled })))
}

/// `CreateMutexA(attributes, initialOwner, name)`: owned by the creating thread
/// when the guest asked for that, which is what a single-instance check does.
fn create_mutex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let owner = if call.arg(1)? != 0 {
        host.current_thread
    } else {
        NO_OWNER
    };
    Ok(Return(host.objects.insert(Object::Mutex { owner })))
}

/// `OpenMutexA(access, inherit, name)`: no mutex of that name exists, so a
/// program checking whether it is already running finds that it is not.
fn open_mutex(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.last_error = ERROR_FILE_NOT_FOUND;
    Ok(Return(0))
}

/// `ReleaseMutex(handle)`: the owning thread gives it up, and the longest wait
/// takes it.
fn release_mutex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    if !host.objects.release(handle, host.current_thread) {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(Return(0));
    }
    host.woken.extend(host.objects.wake(handle));
    Ok(Return(1))
}

/// `CreateThread(attributes, stackSize, startAddress, parameter, flags, threadId)`:
/// a thread this host runs, entered only when the thread that is running cannot
/// go on.
///
/// The handle and the id are made here, because the guest reads the id back out
/// of this very call; the stack and the register set are the session's to build,
/// since a thread's stack is guest memory and a shim has none. The routine is not
/// entered here either: a shim runs on the calling thread's stack, and the new
/// thread starts on its own.
fn create_thread(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let stack_size = call.arg(1)?;
    let entry = call.arg(2)?;
    let parameter = call.arg(3)?;
    if entry == 0 {
        host.last_error = ERROR_INVALID_PARAMETER;
        return Ok(Return(0));
    }
    let (handle, id) = host.objects.insert_thread();
    let thread_id = call.arg(5)?;
    if thread_id != 0 {
        call.write_u32(thread_id, id)?;
    }
    host.spawn = Some(Spawn {
        id,
        handle,
        entry,
        parameter,
        stack_size: if stack_size == 0 {
            None
        } else {
            Some(stack_size)
        },
    });
    Ok(Return(handle))
}

/// `ExitThread(code)`: the running thread ends here, and whoever waits on its
/// handle is released with this code.
fn exit_thread(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.thread_want = Some(ThreadWant::Exit(call.arg(0)?));
    Ok(Return(0))
}

/// `WaitForSingleObject(handle, milliseconds)`.
fn wait_for_single_object(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let timeout = call.arg(1)?;
    wait(host, &[handle], false, timeout)
}

/// `WaitForMultipleObjects(count, handles, waitAll, milliseconds)`.
fn wait_for_multiple_objects(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let count = call.arg(0)?;
    let list = call.arg(1)?;
    let all = call.arg(2)? != 0;
    let timeout = call.arg(3)?;
    if count == 0 || count > MAXIMUM_WAIT_OBJECTS {
        host.last_error = ERROR_INVALID_PARAMETER;
        return Ok(Return(WAIT_FAILED));
    }
    let handles: Vec<u32> = (0..count)
        .map(|index| call.memory.read_u32(list.wrapping_add(index * 4)))
        .collect::<Result<_>>()?;
    wait(host, &handles, all, timeout)
}

/// The one place a wait is decided: answered here when the object is already
/// signalled, and parked on the calling thread when it is not.
///
/// A handle this host keeps no object for is answered as signalled. That is what
/// it was before threads existed -- nothing here can clear it -- and it is right
/// for the handles a file, a console or a device event hands out, which this host
/// never blocks on anyway.
fn wait(host: &mut Host, handles: &[u32], all: bool, timeout: u32) -> Result<Action> {
    let log = std::env::var("SHIMKIT_LOG_OBJECTS").is_ok();
    if log {
        println!(
            "wait thread {} on {handles:?} timeout {timeout}",
            host.current_thread
        );
    }
    if !host.objects.known(handles) {
        return Ok(Return(WAIT_OBJECT_0));
    }
    let thread = host.current_thread;
    if let Some(answer) = host.objects.take_now(thread, handles, all) {
        if log {
            println!("  answered {answer:#x}");
        }
        return Ok(Return(answer));
    }
    if timeout == 0 {
        return Ok(Return(WAIT_TIMEOUT));
    }
    // The thread is parked and the scheduler runs someone else; the session
    // answers the wait, so the value returned here is only what a guest would see
    // if the wait had been satisfied at once.
    if log {
        println!("  parked");
    }
    host.objects.park(thread, handles.to_vec(), all);
    host.thread_want = Some(ThreadWant::Wait {
        timeout: (timeout != INFINITE).then_some(timeout),
    });
    Ok(Return(WAIT_OBJECT_0))
}

/// `SetEvent(handle)`: raises the flag and releases what waits on it. The
/// automatic kind clears itself as the wait takes it.
fn set_event(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    if std::env::var("SHIMKIT_LOG_OBJECTS").is_ok() {
        println!("SetEvent {handle}");
    }
    if !host.objects.signal(handle) {
        // Not an event this host knows; the guest's own numbering, so the call
        // succeeds the way the rest of this file's "done" imports do.
        return Ok(Return(1));
    }
    host.woken.extend(host.objects.wake(handle));
    Ok(Return(1))
}

fn get_tick_count(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // Milliseconds since the page loaded, which is the same clock the timers and
    // `timeGetTime` use.
    Ok(Return(host.now_ms() as u32))
}

fn global_alloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let size = call.arg(1)?;
    match host.allocate(size, 8) {
        Ok(pointer) => Ok(Return(pointer)),
        Err(error) => Ok(Return(refuse(host, call, "GlobalAlloc", size, error))),
    }
}

fn global_size(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.allocated(call.arg(0)?).unwrap_or(0)))
}

/// `GetCurrentDirectoryW(len, buffer)`: the same `"."`, in UTF-16.
fn get_current_directory_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let capacity = call.arg(0)?;
    let buffer = call.arg(1)?;
    if buffer != 0 && capacity >= 2 {
        call.write_u16(buffer, b'.' as u16)?;
        call.write_u16(buffer + 2, 0)?;
        return Ok(Return(1));
    }
    Ok(Return(2))
}

/// `GetUserDefaultLangID()`: this host answers for an English Windows.
///
/// It is the language `GetACP`'s 1252 code page belongs to, so a guest that asks
/// both gets a pair that agree -- a program that picks a font or a code page from
/// the language does not end up with two answers that contradict each other.
fn get_user_default_lang_id(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0x0409))
}

fn get_current_directory(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The bundle is the working directory: names in the guest's own paths are
    // matched by basename, so `"."` resolves to it. Note the order -- this call
    // takes the length first, which is not how most Win32 buffer calls read.
    let capacity = call.arg(0)?;
    let buffer = call.arg(1)?;
    if buffer != 0 && capacity >= 2 {
        call.write(buffer, b".\0")?;
        return Ok(Return(1));
    }
    Ok(Return(2))
}

fn set_current_directory(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn lstrlen(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = read_bytes_until_nul(call.memory, call.arg(0)?)?;
    Ok(Return(text.len() as u32))
}

// ---- critical sections and thread-local storage --------------------------

// A critical section is the guest's own 24-byte structure, so the lock behind
// it is kept here, named by the address of that structure: the guest never sees
// a handle for it. Windows' critical sections are recursive and owned by a
// thread, and both matter once there is more than one -- a list the guest guards
// with one is exactly what a worker thread reads while the main thread builds.

/// `InitializeCriticalSection(critical)`: the structure stays the guest's, and
/// an unowned lock is put behind its address.
fn initialize_critical_section(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    host.critical_depth.remove(&address);
    host.objects.hold(address, Object::Mutex { owner: NO_OWNER });
    Ok(Return(1))
}

/// `EnterCriticalSection(critical)`: the owning thread enters again without
/// waiting, and any other thread is parked until the owner leaves. The wait is a
/// real one, so the session can see the thread standing still and answer it if
/// nothing can ever release it.
fn enter_critical_section(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    let thread = host.current_thread;
    // A guest that enters one it never initialised still gets exclusion: the
    // lock is created where it is first wanted, which is the only honest guess.
    if !host.objects.known(&[address]) {
        host.objects.hold(address, Object::Mutex { owner: NO_OWNER });
    }
    if host.objects.take(address, thread) {
        *host.critical_depth.entry(address).or_insert(0) += 1;
        return Ok(Return(0));
    }
    host.objects.park(thread, vec![address], false);
    host.thread_want = Some(ThreadWant::Wait { timeout: None });
    Ok(Return(WAIT_OBJECT_0))
}

/// `LeaveCriticalSection(critical)`: one level of the owner's nesting goes, and
/// the last one hands the lock to the thread that has waited longest.
fn leave_critical_section(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    let thread = host.current_thread;
    if let Some(depth) = host.critical_depth.get_mut(&address) {
        if *depth > 1 {
            *depth -= 1;
            return Ok(Return(0));
        }
    }
    host.critical_depth.remove(&address);
    host.objects.release(address, thread);
    let woken = host.objects.wake(address);
    if !woken.is_empty() {
        *host.critical_depth.entry(address).or_insert(0) = 1;
        host.woken.extend(woken);
    }
    Ok(Return(0))
}

fn delete_critical_section(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    host.critical_depth.remove(&address);
    host.objects.close(address);
    Ok(Return(0))
}

fn tls_alloc(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.tls.alloc()))
}

fn tls_free(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.tls.free(call.arg(0)?);
    Ok(Return(1))
}

fn tls_set_value(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.tls.set(call.arg(0)?, call.arg(1)?);
    Ok(Return(1))
}

fn tls_get_value(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.tls.get(call.arg(0)?)))
}

fn output_debug_string_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = read_wide(call, call.arg(0)?, u32::MAX)?;
    if !text.is_empty() {
        let clipped: String = text.chars().take(200).collect();
        host.notice(format!("output: {clipped}"));
    }
    Ok(Return(0))
}

fn get_command_line(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.command_line(call.memory)?))
}

/// `GetModuleHandleA(name)`.
///
/// A name this host answers for -- one of the DLLs the shim tables are filed
/// under -- is a module whether or not the guest imported anything from it, which
/// is what a program that resolves its own imports needs. A name no table knows is
/// absent, as it would be on Windows.
fn get_module_handle(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(0)?;
    if name == 0 {
        // `GetModuleHandleA(NULL)` is the program itself.
        return Ok(Return(IMAGE_BASE));
    }
    let text = call.memory.read_cstr(name)?;
    if text.eq_ignore_ascii_case(host.target.exe_name) {
        return Ok(Return(IMAGE_BASE));
    }
    match module_handle(host, &text) {
        Some(handle) => Ok(Return(handle)),
        None => {
            host.last_error = ERROR_MOD_NOT_FOUND;
            Ok(Return(0))
        }
    }
}

/// `GetModuleHandleW(name)`: the same question with a wide name.
fn get_module_handle_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(0)?;
    if name == 0 {
        return Ok(Return(IMAGE_BASE));
    }
    let text = call.wstr(name)?;
    if text.eq_ignore_ascii_case(host.target.exe_name) {
        return Ok(Return(IMAGE_BASE));
    }
    match module_handle(host, &text) {
        Some(handle) => Ok(Return(handle)),
        None => {
            host.last_error = ERROR_MOD_NOT_FOUND;
            Ok(Return(0))
        }
    }
}

/// Where a module handle for a DLL this host answers for starts.
///
/// It is not an image base: the DLL has no image here, only a table of shims. The
/// guest never dereferences a handle -- the only thing that takes one back is
/// `GetProcAddress` -- so an index in a range of the address space nothing else
/// uses is a handle. Below the resolved-name range, above every guest image.
const MODULE_BASE: u32 = 0x00b0_0000;

/// `ERROR_MOD_NOT_FOUND`.
const ERROR_MOD_NOT_FOUND: u32 = 126;
/// `ERROR_PROC_NOT_FOUND`.
const ERROR_PROC_NOT_FOUND: u32 = 127;

/// The handle for a DLL name, allocated on first use so two calls agree.
fn module_handle(host: &mut Host, name: &str) -> Option<u32> {
    // A path loads the file it names, and what this host answers for is the
    // file's basename: `C:\Game\dinput8.dll` is the same DLL as `dinput8.dll`,
    // because the host has no files, only names it keeps tables for.
    let leaf = name.rsplit(['\\', '/']).next().unwrap_or(name);
    if !crate::shim::module_known(host.target.shims, leaf) {
        return None;
    }
    if let Some(index) = host
        .modules
        .iter()
        .position(|known| known.eq_ignore_ascii_case(leaf))
    {
        return Some(MODULE_BASE + index as u32);
    }
    host.modules.push(leaf.to_string());
    Some(MODULE_BASE + (host.modules.len() - 1) as u32)
}

/// The DLL a module handle stands for.
fn module_name(host: &Host, handle: u32) -> Option<&str> {
    let index = handle.checked_sub(MODULE_BASE)? as usize;
    host.modules.get(index).map(String::as_str)
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

fn get_module_file_name_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let buffer = call.arg(1)?;
    let capacity = call.arg(2)?;
    if buffer == 0 || capacity == 0 {
        return Ok(Return(0));
    }
    let units: Vec<u16> = host.target.exe_name.encode_utf16().collect();
    let written = (capacity as usize - 1).min(units.len());
    for (index, unit) in units[..written].iter().enumerate() {
        call.write_u16(buffer + index as u32 * 2, *unit)?;
    }
    call.write_u16(buffer + written as u32 * 2, 0)?;
    Ok(Return(written as u32))
}

fn get_module_handle_ex(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // GET_MODULE_HANDLE_EX_FLAG_*: this host has one module, so every question
    // about it is answered by the image base it maps at.
    Ok(Return(1))
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

fn get_startup_info_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let info = call.arg(0)?;
    if info != 0 {
        // STARTUPINFOW has the same 68-byte shape as the ANSI one on 32-bit.
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

/// `Sleep(ms)`: a yield, and nothing else.
///
/// A thread that sleeps really does lose that much time, so the guest's own wait
/// loop (`while (GetTickCount() - t < 20) Sleep(1);`) is how a game keeps its
/// frame rate. Moving the clock forward here instead of waiting looked free and
/// was not: the loop then ends after a handful of cheap instructions rather than
/// after 20 ms, so the game runs at whatever rate the interpreter can manage and
/// every fade, timer and animation written against `GetTickCount` runs with it.
/// Cave Story's clock measured five times real time that way.
///
/// Nothing is moved. The embedder is the clock, it pushes real time before every
/// chunk, and the guest spins until that clock says the wait is over -- the same
/// loop, reading the same time, just no longer lied to.
///
/// A sleeping thread now has somewhere to put the time: it hands the CPU over to
/// whatever else can run, exactly as this import always did, and comes back when
/// the others are done or have stopped. It is deliberately not a park with a
/// deadline: a guest that sleeps a millisecond per frame would then wait for the
/// embedder's next chunk instead, and every one of those loops is written against
/// a clock this host does not own.
/// `Sleep(milliseconds)`: the thread stands still for that much of the guest's
/// clock. A worker that polls on a sleep needs it: answered at once, a 17 ms
/// poll becomes a spin that runs its work thousands of times before the thread
/// that posted it has moved, which is a race the real kernel never has. Zero
/// milliseconds only gives way, as Windows does.
fn sleep(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let milliseconds = call.arg(0)?;
    if milliseconds == 0 {
        host.thread_want = Some(ThreadWant::Yield);
        return Ok(Return(0));
    }
    // A waiter with no handle is a sleep: nothing can signal it, and its
    // deadline is what ends it.
    host.objects.park(host.current_thread, Vec::new(), false);
    host.thread_want = Some(ThreadWant::Wait {
        timeout: Some(milliseconds),
    });
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

fn are_file_apis_ansi(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
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

fn get_file_attributes(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = call.cstr(0)?;
    // A name the table holds is an ordinary file; anything else is absent, which
    // is the answer the CRT's `stat` turns into a missing path.
    Ok(Return(if host.files.exists(&path) {
        0x80 // FILE_ATTRIBUTE_NORMAL
    } else {
        INVALID_HANDLE_VALUE
    }))
}

fn create_directory(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The host has no writable tree, and a save directory that cannot be made
    // would stop the game at start-up. The directory call succeeds and the file
    // create that follows reports the truth about writing.
    let _ = call.cstr(0)?;
    Ok(Return(1))
}

fn create_file_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = read_wide(call, call.arg(0)?, u32::MAX)?;
    // The wide name is decoded here and the open itself is the one the ANSI
    // entry point uses: one file table, one set of answers.
    Ok(Return(crate::shim::files::open_path(
        host,
        &path,
        call.arg(1)?,
        call.arg(4)?,
    )))
}

fn find_next_file_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    crate::shim::files::find_next_file_w(host, call)
}

fn find_next_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    crate::shim::files::find_next_file(host, call)
}

fn set_file_pointer_ex(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    crate::shim::files::set_file_pointer_ex(host, call)
}

/// `GetFileSize(handle, &high)`: the length the file table knows.
///
/// A guest that reads a whole file into memory asks this first, so the answer
/// has to be the real length; the high half is zero because nothing here is over
/// 4 GB.
fn get_file_size(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let high = call.arg(1)?;
    let Some(length) = host.files.length(handle) else {
        host.last_error = ERROR_INVALID_HANDLE;
        return Ok(Return(INVALID_FILE_SIZE));
    };
    if high != 0 {
        call.write_u32(high, 0)?;
    }
    Ok(Return(length))
}

/// File mappings (`CreateFileMappingA` + `MapViewOfFile`): the objects the
/// guest created and the views it holds, so `UnmapViewOfFile` frees what a view
/// borrowed. A mapping of a real file carries the file's bytes; a page-file
/// mapping (`INVALID_HANDLE_VALUE`) is a zeroed region the guest shares.
#[derive(Default)]
pub struct Mappings {
    mappings: HashMap<u32, Mapping>,
    views: HashMap<u32, u32>,
    next: u32,
}

struct Mapping {
    /// The file's bytes, or empty for a page-file mapping.
    bytes: Vec<u8>,
    /// The bytes the mapping covers, which a view of zero length takes whole.
    size: u32,
}

/// The handle range file mappings live in: distinct from files, objects and
/// everything else that hands out numbers.
const FIRST_MAPPING: u32 = 0x0071_0000;

impl Mappings {
    /// Drops a mapping handle, which `CloseHandle` asks for. Any view already
    /// taken stays mapped until `UnmapViewOfFile` releases it.
    pub fn close(&mut self, handle: u32) -> bool {
        self.mappings.remove(&handle).is_some()
    }
}

fn create_file_mapping_a(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    let size_high = call.arg(3)?;
    let size_low = call.arg(4)?;
    let requested = ((size_high as u64) << 32) | u64::from(size_low);
    let (bytes, size) = if file == INVALID_HANDLE_VALUE {
        (Vec::new(), u32::try_from(requested).unwrap_or(u32::MAX))
    } else {
        let contents = host
            .files
            .open_contents(file)
            .ok_or_else(|| "CreateFileMappingA on a file this host never opened".to_string())?;
        // A zero maximum size maps the file as it stands, which is how a caller
        // says "the whole thing" without knowing its length.
        let size = if requested == 0 {
            contents.len() as u32
        } else {
            u32::try_from(requested)
                .unwrap_or(contents.len() as u32)
                .min(contents.len() as u32)
        };
        (contents, size)
    };
    host.mappings.next += 1;
    let handle = FIRST_MAPPING + host.mappings.next * 4;
    host.mappings
        .mappings
        .insert(handle, Mapping { bytes, size });
    Ok(Return(handle))
}

fn map_view_of_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let mapping_handle = call.arg(0)?;
    let offset_high = call.arg(2)?;
    let offset_low = call.arg(3)?;
    let bytes = call.arg(4)?;
    let Some(mapping) = host.mappings.mappings.get(&mapping_handle) else {
        return Err("MapViewOfFile on a mapping this host never made".into());
    };
    let offset = ((offset_high as u64) << 32) | u64::from(offset_low);
    let (contents, mapped) = (mapping.bytes.clone(), mapping.size);
    drop(mapping);
    let view = if bytes == 0 {
        mapped.saturating_sub(offset as u32)
    } else {
        bytes
    };
    let address = host.allocate(view, 0x1000)?;
    // A fresh view reads as zeroes past the file's end, which is what Windows
    // answers; the arena recycles bytes, so the tail is written explicitly.
    let from = offset as usize;
    let take = contents.len().saturating_sub(from).min(view as usize);
    if take > 0 {
        call.memory.write_bytes(address, &contents[from..from + take])?;
    }
    if (take as u32) < view {
        call.zero(address + take as u32, view - take as u32)?;
    }
    host.mappings.views.insert(address, view);
    Ok(Return(address))
}

fn unmap_view_of_file(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = call.arg(0)?;
    let Some(view) = host.mappings.views.remove(&address) else {
        return Err("UnmapViewOfFile on an address this host never mapped".into());
    };
    host.release(address);
    let _ = view;
    Ok(Return(1))
}

fn close_handle(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Closing a file handle drops it from the table, and closing a kernel object
    // drops it and everything parked on it. Every other handle in this host is a
    // number with no resource behind it, so the close succeeds and the number is
    // simply forgotten.
    let handle = call.arg(0)?;
    crate::shim::files::close(host, handle);
    host.objects.close(handle);
    host.mappings.close(handle);
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
    let pointer = call.arg(2)?;
    // `SHIMKIT_LOG_FREE=<hex>` names a region: every block released inside it is
    // printed with the call site that released it, which is how a structure a
    // guest is still walking gets noticed.
    if let Some(base) = watched_free_region() {
        if pointer.wrapping_sub(base) < 0x2_0000 {
            let caller = call.memory.read_u32(call.args.wrapping_sub(4)).unwrap_or(0);
            println!("free {pointer:#010x} from {caller:#010x}");
        }
    }
    host.release(pointer);
    Ok(Return(1))
}

/// The base of the region `SHIMKIT_LOG_FREE` names, or `None` when it is unset.
fn watched_free_region() -> Option<u32> {
    let text = std::env::var("SHIMKIT_LOG_FREE").ok()?;
    u32::from_str_radix(text.trim_start_matches("0x"), 16).ok()
}

fn heap_size(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The block's usable size is what the arena recorded for it; a pointer the
    // arena never handed out has none.
    Ok(Return(host.allocated(call.arg(2)?).unwrap_or(0)))
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

/// `LoadLibraryA(name)`: a handle for a DLL this host answers for, and a failure
/// for one it does not -- a library that is not here is exactly what the guest
/// sees on a machine without it.
fn load_library(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(0)?;
    if name == 0 {
        host.last_error = ERROR_MOD_NOT_FOUND;
        return Ok(Return(0));
    }
    let text = call.memory.read_cstr(name)?;
    if std::env::var("SHIMKIT_LOG_USER").is_ok() && host.notices.len() < 600 {
        host.notice(format!("LoadLibraryA {text:?}"));
    }
    match module_handle(host, &text) {
        Some(handle) => Ok(Return(handle)),
        None => {
            host.last_error = ERROR_MOD_NOT_FOUND;
            Ok(Return(0))
        }
    }
}

/// `LoadLibraryW(name)`: the same with a wide name.
fn load_library_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(0)?;
    if name == 0 {
        host.last_error = ERROR_MOD_NOT_FOUND;
        return Ok(Return(0));
    }
    let text = call.wstr(name)?;
    match module_handle(host, &text) {
        Some(handle) => Ok(Return(handle)),
        None => {
            host.last_error = ERROR_MOD_NOT_FOUND;
            Ok(Return(0))
        }
    }
}

/// `GetProcAddress(module, name)`: an address the guest may call.
///
/// The address is not code. It is a slot in [`crate::cpu::PROC_BASE`], where the
/// step loop recognises it and runs the binding the name stands for -- the only
/// way a function the program never imported can be answered, since there is no
/// import slot to point at. The same name always gets the same address, which a
/// program that compares two resolutions relies on.
fn get_proc_address(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let module = call.arg(0)?;
    let name = call.arg(1)?;
    // An ordinal arrives as a small integer rather than a string, and the tables
    // file the few ordinals that matter under their `#n` name.
    let wanted = if name < 0x1_0000 {
        format!("#{name}")
    } else {
        call.memory.read_cstr(name)?
    };
    if std::env::var("SHIMKIT_LOG_USER").is_ok() && host.notices.len() < 600 {
        host.notice(format!(
            "GetProcAddress({module:#x}, {wanted:?})"
        ));
    }
    let dll = if module == IMAGE_BASE {
        // The guest's own handle: a program that is its own table of exports is
        // answered out of whichever table holds the name.
        None
    } else {
        match module_name(host, module) {
            Some(dll) => Some(dll.to_string()),
            None => {
                host.last_error = ERROR_PROC_NOT_FOUND;
                return Ok(Return(0));
            }
        }
    };
    let Some(binding) = crate::shim::named(host.target.shims, dll.as_deref(), &wanted) else {
        host.last_error = ERROR_PROC_NOT_FOUND;
        return Ok(Return(0));
    };
    let index = match host
        .procs
        .iter()
        .position(|resolved| std::ptr::eq(*resolved, binding))
    {
        Some(index) => index,
        None => {
            if host.procs.len() as u32 >= crate::cpu::PROC_LIMIT {
                host.last_error = ERROR_PROC_NOT_FOUND;
                return Ok(Return(0));
            }
            host.procs.push(binding);
            host.procs.len() - 1
        }
    };
    Ok(Return(crate::cpu::PROC_BASE + index as u32 * 4))
}

// ---- strings and codepages ----------------------------------------------

fn get_process_heap(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // Every heap is the one arena, so the process heap only has to be a handle
    // HeapAlloc will accept; `heap_alloc` never reads it.
    Ok(Return(1))
}

// ---- console -------------------------------------------------------------

fn get_console_cp(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(437))
}

fn get_console_mode(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let mode = call.arg(1)?;
    if mode != 0 {
        // ENABLE_PROCESSED_INPUT | ENABLE_LINE_INPUT.
        call.write_u32(mode, 3)?;
    }
    Ok(Return(1))
}

fn read_console_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let read = call.arg(3)?;
    if read != 0 {
        call.write_u32(read, 0)?;
    }
    Ok(Return(0))
}

fn write_console_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let written = call.arg(3)?;
    if written != 0 {
        call.write_u32(written, 0)?;
    }
    Ok(Return(1))
}

fn get_acp(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1252))
}

fn get_oem_cp(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(437))
}

fn is_valid_code_page(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The CRT probes the ANSI and OEM code pages before it uses them. Both are
    // answers this host has (1252 and 437), so any page it asks about is a real
    // one as far as it needs to know.
    let _ = call.arg(0)?;
    Ok(Return(1))
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

/// `lstrcpynA(destination, source, maximum)`: at most `maximum` bytes, always
/// NUL-terminated when `maximum` is at least one. The return value is the
/// destination, which is what callers chain.
fn lstrcpyn(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let source = call.arg(1)?;
    let maximum = call.arg(2)? as usize;
    if maximum == 0 {
        return Ok(Return(destination));
    }
    let bytes = read_bytes_until_nul(call.memory, source)?;
    let take = bytes.len().min(maximum - 1);
    let mut copied = bytes[..take].to_vec();
    copied.push(0);
    call.write(destination, &copied)?;
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

fn get_system_time_as_file_time(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    if at != 0 {
        // 2000-01-01 12:00:00 UTC, the same instant GetSystemTime reports.
        call.write_u32(at, 0x0D0A_1A00)?;
        call.write_u32(at.wrapping_add(4), 0x01BF_53EB)?;
    }
    Ok(Return(0))
}

fn get_date_format_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    write_fixed_text(call, call.arg(4)?, call.arg(5)?, "2000-01-01")
}

fn get_time_format_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    write_fixed_text(call, call.arg(4)?, call.arg(5)?, "12:00:00")
}

/// Writes a UTF-16 answer with its terminator: the count Windows returns, or 0
/// when the caller's buffer cannot hold it.
fn write_fixed_text(
    call: &mut Call<'_>,
    buffer: u32,
    capacity: u32,
    text: &str,
) -> Result<Action> {
    if buffer == 0 || capacity == 0 {
        return Ok(Return(0));
    }
    let units: Vec<u16> = text.encode_utf16().collect();
    if capacity <= units.len() as u32 {
        return Ok(Return(0));
    }
    for (index, unit) in units.iter().enumerate() {
        call.write_u16(buffer + index as u32 * 2, *unit)?;
    }
    call.write_u16(buffer + units.len() as u32 * 2, 0)?;
    Ok(Return(units.len() as u32 + 1))
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

    /// Runs a shim by name on a blank machine, for the entry points that do not
    /// need an image's import table -- the module and name lookups read their
    /// arguments from the stack like any other shim.
    fn blank_call(
        host: &mut Host,
        memory: &mut GuestMemory,
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("{name} is implemented"));
        let at = STACK_BASE + 0x2000;
        for (index, value) in arguments.iter().enumerate() {
            memory.write_u32(at + index as u32 * 4, *value)?;
        }
        let mut call = Call {
            memory,
            args: at,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call)
    }

    #[test]
    fn a_mapped_file_reads_where_the_view_points() {
        let mut memory = GuestMemory::empty(crate::memory::HEAP_BASE as usize + 0x1000)
            .expect("blank memory");
        let mut host = Host::new();
        // A file the guest opened, which the mapping then covers.
        host.files.provide(r"data\map.bin", b"file bytes here".to_vec());
        let handle = host.files.open(r"data\map.bin").expect("the file opens");

        // A file handle is a value the guest passes straight through, not a
        // pointer into its stack.
        let Return(mapping) =
            blank_call(&mut host, &mut memory, "CreateFileMappingA", &[handle, 0, 0, 0, 0])
                .unwrap()
        else {
            panic!("the file maps")
        };
        assert_ne!(mapping, 0);

        let view = STACK_BASE - 0x160;
        let Return(address) = blank_call(
            &mut host,
            &mut memory,
            "MapViewOfFile",
            &[mapping, 0, 0, 0, 0],
        )
        .unwrap()
        else {
            panic!("the view maps")
        };
        // The view holds the file's bytes, and unmapping releases them.
        let mut bytes = Vec::new();
        for index in 0..15 {
            bytes.push(memory.read_u8(address + index).unwrap());
        }
        assert_eq!(bytes, b"file bytes here");
        assert_eq!(
            blank_call(&mut host, &mut memory, "UnmapViewOfFile", &[address]).unwrap(),
            Return(1)
        );
        assert!(host.allocate(address, 4).is_ok(), "the view's bytes went back");

        // Closing the mapping handle drops it.
        assert_eq!(
            blank_call(&mut host, &mut memory, "CloseHandle", &[mapping]).unwrap(),
            Return(1)
        );
    }

    #[test]
    fn a_program_that_resolves_its_own_imports_gets_addresses_it_can_call() {
        let mut memory = GuestMemory::empty(crate::memory::HEAP_BASE as usize + 0x1000)
            .expect("blank memory");
        let mut host = Host::new();
        let module = STACK_BASE - 0x300;
        let function = STACK_BASE - 0x280;
        let missing = STACK_BASE - 0x240;
        let absent = STACK_BASE - 0x200;
        memory.write_bytes(module, b"USER32.dll\0").unwrap();
        memory.write_bytes(function, b"MessageBoxA\0").unwrap();
        memory.write_bytes(missing, b"NotAFunction\0").unwrap();
        memory.write_bytes(absent, b"OPENGL32.dll\0").unwrap();

        // A DLL the shim tables are filed under is a module, imported or not.
        let Return(handle) =
            blank_call(&mut host, &mut memory, "GetModuleHandleA", &[module]).unwrap()
        else {
            panic!("USER32.dll is a module here")
        };
        assert_ne!(handle, 0);
        assert_eq!(
            blank_call(&mut host, &mut memory, "GetModuleHandleA", &[module]).unwrap(),
            Return(handle),
            "a second query is the same handle"
        );

        // A DLL no table answers for is absent, which is what the guest has to be
        // told before it decides to do without it.
        assert_eq!(
            blank_call(&mut host, &mut memory, "LoadLibraryA", &[absent]).unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_MOD_NOT_FOUND);

        // A path loads the file it names, and the file this host answers for is
        // its basename: the same handle the bare name gets.
        let path = STACK_BASE - 0x1c0;
        memory.write_bytes(path, b"C:\\Game\\USER32.dll\0").unwrap();
        assert_eq!(
            blank_call(&mut host, &mut memory, "LoadLibraryA", &[path]).unwrap(),
            blank_call(&mut host, &mut memory, "GetModuleHandleA", &[module]).unwrap(),
            "a path and its basename are the same module"
        );

        // A name in that module is an address in the resolved-name range, where
        // the step loop runs the binding rather than fetching instructions.
        let Return(address) = blank_call(
            &mut host,
            &mut memory,
            "GetProcAddress",
            &[handle, function],
        )
        .unwrap()
        else {
            panic!("MessageBoxA resolves")
        };
        let index = crate::cpu::proc_at(address).expect("a resolved name has its own address");
        assert_eq!(host.procs[index].name, "MessageBoxA");
        assert_eq!(
            blank_call(&mut host, &mut memory, "GetProcAddress", &[handle, function]).unwrap(),
            Return(address),
            "resolving the same name twice gives the same address"
        );

        // A name the module does not export, and a module that is not a handle.
        assert_eq!(
            blank_call(&mut host, &mut memory, "GetProcAddress", &[handle, missing]).unwrap(),
            Return(0)
        );
        assert_eq!(host.last_error, ERROR_PROC_NOT_FOUND);
        assert_eq!(
            blank_call(&mut host, &mut memory, "GetProcAddress", &[0x1234, function]).unwrap(),
            Return(0)
        );
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
    fn the_interlocked_pair_reports_the_value_it_replaced() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let slot = STACK_BASE + 0x3000;
        memory.write_u32(slot, 7).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "InterlockedExchange", &[slot, 9]).unwrap(),
            Return(7)
        );
        assert_eq!(memory.read_u32(slot).unwrap(), 9);

        // The exchange happens only when the comparand matches, and the answer is
        // the value that was there either way.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "InterlockedCompareExchange",
                &[slot, 11, 9]
            )
            .unwrap(),
            Return(9)
        );
        assert_eq!(memory.read_u32(slot).unwrap(), 11);
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "InterlockedCompareExchange",
                &[slot, 13, 9]
            )
            .unwrap(),
            Return(11)
        );
        assert_eq!(
            memory.read_u32(slot).unwrap(),
            11,
            "a mismatch changes nothing"
        );
    }

    #[test]
    fn locale_fields_answer_the_separators_a_settings_screen_asks_for() {
        // The table is a fact about this machine, so this runs with no image.
        assert_eq!(locale_text(0x0e), Some("."));
        assert_eq!(locale_text(0x0f), Some(","));
        assert_eq!(locale_text(0x59), Some("en"));
        assert_eq!(locale_text(0x5a), Some("US"));
        assert_eq!(locale_text(0x2a), Some("Monday"));
        assert_eq!(locale_text(0x38), Some("January"));
        assert_eq!(locale_text(0x44), Some("Jan"));
        assert_eq!(locale_text(0x1003), Some("h:mm:ss tt"));
        assert_eq!(locale_text(0x9999), None, "an unknown field is a failure");
    }

    #[test]
    fn get_locale_info_writes_its_field_and_its_length() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let buffer = STACK_BASE + 0x3100;
        memory.write_bytes(buffer, &[0xaa; 8]).unwrap();
        // The decimal separator is one character, so two bytes with the NUL.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GetLocaleInfoA",
                &[0x0409, 0x0e, buffer, 8]
            )
            .unwrap(),
            Return(2)
        );
        assert_eq!(memory.read_bytes(buffer, 2).unwrap(), b".\0");

        // A null buffer asks for the length alone, terminator included.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "GetLocaleInfoW",
                &[0x0409, 0x5c, 0, 0]
            )
            .unwrap(),
            Return(6)
        );
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
    fn lstrcpyn_stops_at_the_maximum_and_always_terminates() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let source = STACK_BASE + 0x2200;
        let destination = STACK_BASE + 0x2400;
        memory.write_bytes(source, b"abcdef\0").unwrap();
        memory.write_bytes(destination, &[0xaa; 8]).unwrap();

        // Four bytes of room: three characters and the terminator, and the
        // bytes past it are left as they were.
        assert_eq!(
            call_shim(&mut host, &mut memory, "lstrcpynA", &[destination, source, 4]).unwrap(),
            Return(destination)
        );
        assert_eq!(memory.read_bytes(destination, 5).unwrap(), b"abc\0\xaa");

        // Enough room copies the whole string.
        assert_eq!(
            call_shim(&mut host, &mut memory, "lstrcpynA", &[destination, source, 16]).unwrap(),
            Return(destination)
        );
        assert_eq!(memory.read_bytes(destination, 8).unwrap(), b"abcdef\0\x00");

        // A zero maximum writes nothing at all.
        assert_eq!(
            call_shim(&mut host, &mut memory, "lstrcpynA", &[destination, source, 0]).unwrap(),
            Return(destination)
        );
        assert_eq!(memory.read_bytes(destination, 7).unwrap(), b"abcdef\0");
    }

    #[test]
    fn the_version_info_is_windows_xp_within_the_size_the_caller_gave() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let info = STACK_BASE + 0x2600;
        memory.write_u32(info, 148).unwrap();
        memory.write_bytes(info + 20, &[0xaa; 8]).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetVersionExA", &[info]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(info + 4).unwrap(), 5, "major");
        assert_eq!(memory.read_u32(info + 8).unwrap(), 1, "minor");
        assert_eq!(memory.read_u32(info + 12).unwrap(), 2600, "build");
        assert_eq!(memory.read_u32(info + 16).unwrap(), 2, "NT platform");
        assert_eq!(memory.read_bytes(info + 20, 8).unwrap(), [0; 8], "service pack text");

        // A short buffer gets the fields that fit and nothing past its end.
        let short = STACK_BASE + 0x2700;
        memory.write_u32(short, 8).unwrap();
        memory.write_u32(short + 8, 0xaaaa_aaaa).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "GetVersionExA", &[short]).unwrap(),
            Return(1)
        );
        assert_eq!(memory.read_u32(short + 4).unwrap(), 5);
        assert_eq!(memory.read_u32(short + 8).unwrap(), 0xaaaa_aaaa, "untouched");
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

    /// A machine with no image, for the calls a guest makes before it has one: a
    /// blank window is memory enough for the handles and the ids they write.
    fn blank() -> Option<(Host, GuestMemory)> {
        let memory = GuestMemory::empty(crate::memory::HEAP_BASE as usize + 0x1000).ok()?;
        Some((Host::new(), memory))
    }

    #[test]
    fn a_worker_waits_for_a_release_and_a_join_comes_back_when_it_ends() {
        let Some((mut host, mut memory)) = blank() else {
            return;
        };
        let id_at = STACK_BASE + 0x3000;

        // A semaphore with no count, and a worker that waits on it. The thread is
        // the session's to make -- its stack is guest memory -- so the shim only
        // says what it would be.
        let Return(semaphore) =
            blank_call(&mut host, &mut memory, "CreateSemaphoreA", &[0, 0, 4, 0]).unwrap()
        else {
            panic!("a semaphore is a handle")
        };
        let Return(worker) = blank_call(
            &mut host,
            &mut memory,
            "CreateThread",
            &[0, 0, 0x0040_1000, 7, 0, id_at],
        )
        .unwrap()
        else {
            panic!("a thread is a handle")
        };
        let spawn = host.spawn.take().expect("a thread is created");
        assert_eq!(
            (spawn.id, spawn.handle, spawn.entry, spawn.parameter),
            (1, worker, 0x0040_1000, 7)
        );
        assert_eq!(
            memory.read_u32(id_at).unwrap(),
            1,
            "the guest is told the id under the same call"
        );

        // Waiting on a count that was never raised parks the thread that asked,
        // and tells the session why.
        host.current_thread = spawn.id;
        assert_eq!(
            blank_call(&mut host, &mut memory, "WaitForSingleObject", &[semaphore, INFINITE])
                .unwrap(),
            Return(WAIT_OBJECT_0)
        );
        assert_eq!(host.thread_want, Some(ThreadWant::Wait { timeout: None }));
        assert_eq!(host.objects.waiting(), vec![(spawn.id, vec![semaphore])]);

        // A wait that has already run out does not park at all.
        host.thread_want = None;
        assert_eq!(
            blank_call(&mut host, &mut memory, "WaitForSingleObject", &[semaphore, 0]).unwrap(),
            Return(WAIT_TIMEOUT)
        );
        assert_eq!(host.thread_want, None);

        // The main thread releases it: the count goes to the waiting thread, and
        // the session is told which one to make runnable.
        host.current_thread = 0;
        assert_eq!(
            blank_call(&mut host, &mut memory, "ReleaseSemaphore", &[semaphore, 1, 0]).unwrap(),
            Return(1)
        );
        assert_eq!(host.woken, vec![(spawn.id, WAIT_OBJECT_0)]);

        // The worker ends, which is what a join waits for.
        host.current_thread = spawn.id;
        assert_eq!(
            blank_call(&mut host, &mut memory, "ExitThread", &[3]).unwrap(),
            Return(0)
        );
        assert_eq!(host.thread_want.take(), Some(ThreadWant::Exit(3)));
        assert_eq!(host.objects.thread_exited(spawn.id, 3), vec![worker]);
        assert!(
            host.objects.take(worker, 0),
            "a thread that has exited is signalled"
        );
        assert_eq!(
            host.objects.get(worker),
            Some(Object::Thread {
                id: 1,
                exited: true,
                exit_code: 3
            })
        );
    }

    #[test]
    fn an_automatic_event_clears_itself_for_the_wait_that_takes_it() {
        let Some((mut host, mut memory)) = blank() else {
            return;
        };
        // `CreateEventA(attributes, manualReset, initialState, name)`.
        let Return(event) =
            blank_call(&mut host, &mut memory, "CreateEventA", &[0, 0, 1, 0]).unwrap()
        else {
            panic!("an event is a handle")
        };

        // It was created set, so the first wait takes it and the second parks.
        assert_eq!(
            blank_call(&mut host, &mut memory, "WaitForSingleObject", &[event, INFINITE]).unwrap(),
            Return(WAIT_OBJECT_0)
        );
        assert_eq!(host.thread_want, None, "a wait that can be answered does not park");
        host.thread_want = None;
        assert_eq!(
            blank_call(&mut host, &mut memory, "WaitForSingleObject", &[event, INFINITE]).unwrap(),
            Return(WAIT_OBJECT_0)
        );
        assert_eq!(host.thread_want, Some(ThreadWant::Wait { timeout: None }));

        // `SetEvent` raises it again and hands it to whoever was waiting.
        host.thread_want = None;
        assert_eq!(
            blank_call(&mut host, &mut memory, "SetEvent", &[event]).unwrap(),
            Return(1)
        );
        assert_eq!(host.woken, vec![(0, WAIT_OBJECT_0)]);
    }

    #[test]
    fn a_mutex_goes_to_the_waiting_thread_when_its_owner_releases_it() {
        let Some((mut host, mut memory)) = blank() else {
            return;
        };
        host.current_thread = 1;
        let Return(mutex) =
            blank_call(&mut host, &mut memory, "CreateMutexA", &[0, 1, 0]).unwrap()
        else {
            panic!("a mutex is a handle")
        };
        assert_eq!(host.objects.get(mutex), Some(Object::Mutex { owner: 1 }));

        // Another thread cannot have it, so its wait parks until the owner gives
        // it up.
        host.current_thread = 2;
        host.objects.park(2, vec![mutex], false);
        host.current_thread = 1;
        assert_eq!(
            blank_call(&mut host, &mut memory, "ReleaseMutex", &[mutex]).unwrap(),
            Return(1)
        );
        assert_eq!(host.woken, vec![(2, WAIT_OBJECT_0)]);
        assert_eq!(host.objects.get(mutex), Some(Object::Mutex { owner: 2 }));

        // A release from a thread that does not own it leaves the owner where it
        // is: that is a guest bug, and taking the mutex from its owner over it
        // would be this host inventing one.
        host.current_thread = 3;
        assert_eq!(
            blank_call(&mut host, &mut memory, "ReleaseMutex", &[mutex]).unwrap(),
            Return(1)
        );
        assert_eq!(host.objects.get(mutex), Some(Object::Mutex { owner: 2 }));
    }

    #[test]
    fn a_critical_section_is_recursive_and_parks_another_thread() {
        let Some((mut host, mut memory)) = blank() else {
            return;
        };
        let section = 0x0040_2000;
        assert_eq!(
            blank_call(&mut host, &mut memory, "InitializeCriticalSection", &[section]).unwrap(),
            Return(1)
        );

        // The owner enters twice without waiting, and thread 0 is an owner like
        // any other: zero is the main thread's id, not "nobody".
        host.current_thread = 0;
        for _ in 0..2 {
            assert_eq!(
                blank_call(&mut host, &mut memory, "EnterCriticalSection", &[section]).unwrap(),
                Return(0)
            );
            assert_eq!(host.thread_want, None, "the owner never waits");
        }

        // Another thread is parked on the guest's own structure address, because
        // that is the only name the two calls share.
        host.current_thread = 1;
        assert_eq!(
            blank_call(&mut host, &mut memory, "EnterCriticalSection", &[section]).unwrap(),
            Return(0)
        );
        assert_eq!(host.thread_want.take(), Some(ThreadWant::Wait { timeout: None }));
        assert_eq!(host.objects.waiting(), vec![(1, vec![section])]);

        // The first leave only drops the owner's nesting; the second hands the
        // lock to the thread that was waiting.
        host.current_thread = 0;
        assert_eq!(
            blank_call(&mut host, &mut memory, "LeaveCriticalSection", &[section]).unwrap(),
            Return(0)
        );
        assert_eq!(host.woken, Vec::new());
        assert_eq!(
            blank_call(&mut host, &mut memory, "EnterCriticalSection", &[section]).unwrap(),
            Return(0),
            "still the owner's: a nested leave must not unlock it"
        );
        host.woken.clear();
        blank_call(&mut host, &mut memory, "LeaveCriticalSection", &[section]).unwrap();
        blank_call(&mut host, &mut memory, "LeaveCriticalSection", &[section]).unwrap();
        assert_eq!(host.woken, vec![(1, WAIT_OBJECT_0)]);
    }

    #[test]
    fn sleep_parks_the_thread_until_its_deadline() {
        let Some((mut host, mut memory)) = blank() else {
            return;
        };
        host.current_thread = 1;
        assert_eq!(
            blank_call(&mut host, &mut memory, "Sleep", &[17]).unwrap(),
            Return(0)
        );
        assert_eq!(
            host.thread_want.take(),
            Some(ThreadWant::Wait { timeout: Some(17) })
        );
        // A sleeper is a waiter with nothing to wait on: the session's clock is
        // what ends it, and no signal can.
        assert_eq!(host.objects.waiting(), vec![(1, Vec::new())]);

        // Zero only gives way, so a guest that yields in a loop keeps running.
        assert_eq!(
            blank_call(&mut host, &mut memory, "Sleep", &[0]).unwrap(),
            Return(0)
        );
        assert_eq!(host.thread_want.take(), Some(ThreadWant::Yield));
    }
}
