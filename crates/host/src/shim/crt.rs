//! The C runtime: what runs before `main`, and the primitives it runs on.
//!
//! A program built with Visual C++ does not begin at `main`. Its entry point is
//! `WinMainCRTStartup`, a routine the C runtime supplies, which sets the
//! application type, installs the maths-error handler, sets the floating-point
//! control word, asks for `argc` and `argv`, walks the table of C++ static
//! initialisers, calls `main`, and finally `exit`s. Every call this module answers
//! belongs to that prologue or to the handful of C library functions an
//! initialiser touches before the first frame of the game.
//!
//! # One table for a family of DLLs
//!
//! The C runtime has shipped under many names: `msvcrt.dll` in every Windows
//! release since NT, and `msvcr60`, `msvcr71`, `msvcr80`, `msvcr100` when a
//! program carried a copy beside its executable. The entry points are the same in
//! all of them, so this table is filed under [`MSVCRT`] and
//! [`crate::shim::binding`] treats the whole family as that one name.
//!
//! The generations do add entry points, though, and a program built against a
//! later one calls them from its own start-up: `_controlfp_s` instead of
//! `_controlfp`, the four-argument `_except_handler4_common` and
//! `__CxxFrameHandler3` for its exception frames, `_ismsbblead` for its case
//! conversions. Those are answered here too, so a `msvcr80` start-up runs the same
//! way a `msvcrt` one does; `_acmdln` is a data import and is filled by
//! [`fill_data_slots`]. The C++ half (`msvcp60` and its successors) is not
//! answered here: its functions are member functions over MSVC's own object
//! layouts, which is different work.
//!
//! # Why `_initterm` and `exit` can call the guest more than once
//!
//! Both run a list of guest routines -- static initialisers, `atexit` handlers --
//! and a shim cannot call guest code in a loop: it runs inside the interpreter,
//! with the guest's frame suspended underneath. So it remembers where the list
//! has got to, asks the session for one entry (`GuestCall` with `resume` set),
//! and the session runs the shim again when that entry returns. The import is
//! only finished once the whole list has run.
//!
//! A resumed shim reads its arguments from [`Host`] state, never from
//! [`Call::arg`]: the guest routine's own frame was pushed over the import's
//! arguments, so the stack window it was called with no longer holds them.

use crate::memory::GuestMemory;
use crate::shim::{Action, Binding, Call, GuestCall, Host};
use crate::Result;

use Action::Return;

/// The name this table is filed under. Every C runtime in the `msvcrt` lineage
/// answers from it; see [`crate::shim::binding`].
pub const MSVCRT: &str = "MSVCRT.dll";

/// The x87 control word Windows gives a new thread: round to nearest, 53-bit
/// precision, every exception masked. This machine's FPU has no mode of its own
/// for `_controlfp` to change, so it reports the default and leaves the guest's
/// own `fldcw` alone.
const DEFAULT_CONTROL_WORD: u32 = 0x027f;

/// `_EXIT_FAILURE`, which `abort` ends the process with.
const EXIT_FAILURE: u32 = 3;

/// The instant this machine reports, in seconds since 1970: 2000-01-01 12:00:00,
/// which is what `GetLocalTime` and `GetSystemTimeAsFileTime` answer, so a
/// program that asks the CRT and the Win32 API agrees with itself.
const FIXED_SECONDS: i64 = 946_728_000;

/// `struct tm`'s shape as VC's `<time.h>` declares it: nine `int`s.
const TM_BYTES: u32 = 36;

pub const SHIMS: &[Binding] = &[
    // ---- start-up ----------------------------------------------------
    Binding { dll: MSVCRT, name: "__set_app_type", arg_bytes: 4, shim: set_app_type },
    Binding { dll: MSVCRT, name: "__setusermatherr", arg_bytes: 4, shim: set_user_math_err },
    Binding { dll: MSVCRT, name: "_adjust_fdiv", arg_bytes: 0, shim: adjust_fdiv },
    Binding { dll: MSVCRT, name: "_controlfp", arg_bytes: 8, shim: controlfp },
    Binding { dll: MSVCRT, name: "__p__fmode", arg_bytes: 0, shim: p_fmode },
    Binding { dll: MSVCRT, name: "__p__commode", arg_bytes: 0, shim: p_commode },
    Binding { dll: MSVCRT, name: "__getmainargs", arg_bytes: 20, shim: main_args },
    Binding { dll: MSVCRT, name: "_initterm", arg_bytes: 8, shim: initterm },
    Binding { dll: MSVCRT, name: "_initterm_e", arg_bytes: 8, shim: initterm_e },
    // ---- process exit ------------------------------------------------
    Binding { dll: MSVCRT, name: "_onexit", arg_bytes: 4, shim: onexit },
    Binding { dll: MSVCRT, name: "atexit", arg_bytes: 4, shim: atexit },
    Binding { dll: MSVCRT, name: "__dllonexit", arg_bytes: 12, shim: dllonexit },
    Binding { dll: MSVCRT, name: "exit", arg_bytes: 4, shim: exit },
    Binding { dll: MSVCRT, name: "_exit", arg_bytes: 4, shim: exit_now },
    Binding { dll: MSVCRT, name: "_cexit", arg_bytes: 0, shim: cexit },
    Binding { dll: MSVCRT, name: "abort", arg_bytes: 0, shim: abort },
    Binding { dll: MSVCRT, name: "_purecall", arg_bytes: 0, shim: purecall },
    Binding { dll: MSVCRT, name: "?terminate@@YAXXZ", arg_bytes: 0, shim: terminate },
    // ---- thread-safe CRT housekeeping --------------------------------
    Binding { dll: MSVCRT, name: "_lock", arg_bytes: 4, shim: lock },
    Binding { dll: MSVCRT, name: "_unlock", arg_bytes: 4, shim: unlock },
    Binding { dll: MSVCRT, name: "_configthreadlocale", arg_bytes: 4, shim: config_thread_locale },
    Binding { dll: MSVCRT, name: "_encode_pointer", arg_bytes: 4, shim: encode_pointer },
    Binding { dll: MSVCRT, name: "_decode_pointer", arg_bytes: 4, shim: decode_pointer },
    // ---- heap --------------------------------------------------------
    Binding { dll: MSVCRT, name: "malloc", arg_bytes: 4, shim: malloc },
    Binding { dll: MSVCRT, name: "calloc", arg_bytes: 8, shim: calloc },
    Binding { dll: MSVCRT, name: "realloc", arg_bytes: 8, shim: realloc },
    Binding { dll: MSVCRT, name: "free", arg_bytes: 4, shim: free },
    Binding { dll: MSVCRT, name: "??2@YAPAXI@Z", arg_bytes: 4, shim: allocate_object },
    Binding { dll: MSVCRT, name: "??_U@YAPAXI@Z", arg_bytes: 4, shim: allocate_object },
    Binding { dll: MSVCRT, name: "??3@YAXPAX@Z", arg_bytes: 4, shim: free_object },
    Binding { dll: MSVCRT, name: "??_V@YAXPAX@Z", arg_bytes: 4, shim: free_object },
    // ---- stdio -------------------------------------------------------
    Binding { dll: MSVCRT, name: "fopen", arg_bytes: 8, shim: fopen },
    Binding { dll: MSVCRT, name: "fclose", arg_bytes: 4, shim: fclose },
    Binding { dll: MSVCRT, name: "fread", arg_bytes: 16, shim: fread },
    Binding { dll: MSVCRT, name: "fwrite", arg_bytes: 16, shim: fwrite },
    Binding { dll: MSVCRT, name: "fseek", arg_bytes: 12, shim: fseek },
    Binding { dll: MSVCRT, name: "ftell", arg_bytes: 4, shim: ftell },
    Binding { dll: MSVCRT, name: "feof", arg_bytes: 4, shim: feof },
    Binding { dll: MSVCRT, name: "ferror", arg_bytes: 4, shim: ferror },
    Binding { dll: MSVCRT, name: "fgets", arg_bytes: 12, shim: fgets },
    // ---- formatted output --------------------------------------------
    Binding { dll: MSVCRT, name: "sprintf", arg_bytes: 8, shim: sprintf },
    Binding { dll: MSVCRT, name: "vsprintf", arg_bytes: 12, shim: vsprintf },
    Binding { dll: MSVCRT, name: "fprintf", arg_bytes: 8, shim: fprintf },
    // ---- formatted input --------------------------------------------
    Binding { dll: MSVCRT, name: "sscanf", arg_bytes: 8, shim: sscanf },
    Binding { dll: MSVCRT, name: "fscanf", arg_bytes: 8, shim: fscanf },
    // ---- the working directory ---------------------------------------
    Binding { dll: MSVCRT, name: "_getcwd", arg_bytes: 8, shim: getcwd },
    Binding { dll: MSVCRT, name: "_chdir", arg_bytes: 4, shim: chdir },
    // ---- the clock ---------------------------------------------------
    Binding { dll: MSVCRT, name: "_time64", arg_bytes: 4, shim: time64 },
    Binding { dll: MSVCRT, name: "_localtime64", arg_bytes: 4, shim: localtime64 },
    // ---- what a VC8 (msvcr80) start-up calls --------------------------
    // These belong to the runtime generations after VC6, and their entry points
    // are the ones its `WinMainCRTStartup` reaches for.
    Binding { dll: MSVCRT, name: "_controlfp_s", arg_bytes: 12, shim: controlfp_s },
    Binding { dll: MSVCRT, name: "_ismbblead", arg_bytes: 4, shim: is_mbblead },
    Binding { dll: MSVCRT, name: "_crt_debugger_hook", arg_bytes: 4, shim: crt_debugger_hook },
    Binding { dll: MSVCRT, name: "_XcptFilter", arg_bytes: 8, shim: xcpt_filter },
    Binding { dll: MSVCRT, name: "_amsg_exit", arg_bytes: 4, shim: amsg_exit },
    Binding { dll: MSVCRT, name: "_invoke_watson", arg_bytes: 20, shim: invoke_watson },
    Binding { dll: MSVCRT, name: "_except_handler4_common", arg_bytes: 24, shim: except_handler },
    Binding { dll: MSVCRT, name: "__CxxFrameHandler3", arg_bytes: 16, shim: cxx_frame_handler },
    // ---- memory and strings -------------------------------------------
    Binding { dll: MSVCRT, name: "memset", arg_bytes: 12, shim: memset },
    Binding { dll: MSVCRT, name: "memcpy", arg_bytes: 12, shim: memcpy },
    Binding { dll: MSVCRT, name: "memmove", arg_bytes: 12, shim: memmove },
    Binding { dll: MSVCRT, name: "memcmp", arg_bytes: 12, shim: memcmp },
    Binding { dll: MSVCRT, name: "strlen", arg_bytes: 4, shim: strlen },
    Binding { dll: MSVCRT, name: "strcpy", arg_bytes: 8, shim: strcpy },
    Binding { dll: MSVCRT, name: "strncpy", arg_bytes: 12, shim: strncpy },
    Binding { dll: MSVCRT, name: "strcat", arg_bytes: 8, shim: strcat },
    Binding { dll: MSVCRT, name: "strcmp", arg_bytes: 8, shim: strcmp },
    Binding { dll: MSVCRT, name: "strncmp", arg_bytes: 12, shim: strncmp },
    Binding { dll: MSVCRT, name: "strchr", arg_bytes: 8, shim: strchr },
    Binding { dll: MSVCRT, name: "strstr", arg_bytes: 8, shim: strstr },
    // ---- floating point ------------------------------------------------
    //
    // These answer on the x87 stack, so each one names an operation the session
    // applies rather than a value in `eax`.
    Binding { dll: MSVCRT, name: "sqrt", arg_bytes: 8, shim: sqrt },
    Binding { dll: MSVCRT, name: "sin", arg_bytes: 8, shim: sin },
    Binding { dll: MSVCRT, name: "cos", arg_bytes: 8, shim: cos },
    Binding { dll: MSVCRT, name: "atan", arg_bytes: 8, shim: atan },
    Binding { dll: MSVCRT, name: "asin", arg_bytes: 8, shim: asin },
    Binding { dll: MSVCRT, name: "fabs", arg_bytes: 8, shim: fabs },
    Binding { dll: MSVCRT, name: "floor", arg_bytes: 8, shim: floor },
    Binding { dll: MSVCRT, name: "pow", arg_bytes: 16, shim: pow },
    Binding { dll: MSVCRT, name: "_CIacos", arg_bytes: 0, shim: ci_acos },
    Binding { dll: MSVCRT, name: "_CIpow", arg_bytes: 0, shim: ci_pow },
    Binding { dll: MSVCRT, name: "_ftol", arg_bytes: 0, shim: ftol },
    // ---- the rest of what an initialiser calls ------------------------
    Binding { dll: MSVCRT, name: "rand", arg_bytes: 0, shim: rand },
    Binding { dll: MSVCRT, name: "srand", arg_bytes: 4, shim: srand },
    Binding { dll: MSVCRT, name: "_finite", arg_bytes: 8, shim: finite },
];

/// State the C runtime keeps between calls.
pub struct Crt {
    /// The `int` `__p__fmode` and `__p__commode` hand out, so the guest can read
    /// and set the default file and device translation modes.
    fmode: Option<u32>,
    commode: Option<u32>,
    /// `_acmdln`'s variable: the slot holds the address of this dword, and the
    /// dword holds the pointer to the command line.
    acmdln: Option<u32>,
    /// `__getmainargs`' answer, kept so a second call returns the same pointers.
    /// `(argc, argv)`.
    argv: Option<(u32, u32)>,
    /// The environment block `__getmainargs` points at.
    environ: Option<u32>,
    /// The `_initterm` table walk that is in the middle of running.
    walk: Option<Walk>,
    /// Handlers registered by `_onexit` and `atexit`, oldest first: `exit` runs
    /// them in reverse.
    atexit: Vec<u32>,
    /// The `exit`/`_cexit` that is running those handlers.
    exiting: Option<Exiting>,
    /// The `rand` sequence. Seeded with 1, as the CRT does.
    seed: u32,
    /// The `FILE` blocks `fopen` handed out, by their guest address.
    streams: std::collections::BTreeMap<u32, Stream>,
    /// The `_file` descriptor the next stream gets. 0, 1 and 2 are the standard
    /// streams, which this host does not hand out.
    next_stream: u32,
    /// The `struct tm` `_localtime64` answers with, allocated on first use: the
    /// CRT returns a pointer into a static block the program may overwrite.
    tm: Option<u32>,
}

/// One open `FILE`.
struct Stream {
    /// The handle in the file table the stream reads and writes through.
    handle: u32,
    /// Whether the stream may read and write, from the mode `fopen` was given.
    readable: bool,
    writable: bool,
    /// `a` mode: every write goes to the end of the file, whatever the cursor
    /// says.
    append: bool,
}

impl Default for Crt {
    fn default() -> Self {
        Self {
            fmode: None,
            commode: None,
            acmdln: None,
            argv: None,
            environ: None,
            walk: None,
            atexit: Vec::new(),
            exiting: None,
            seed: 1,
            streams: std::collections::BTreeMap::new(),
            next_stream: 3,
            tm: None,
        }
    }
}

/// An `_initterm` walk in flight: where it is and how far it goes.
struct Walk {
    /// The next table entry to read.
    next: u32,
    /// One past the table's last entry.
    end: u32,
}

/// An `exit` or `_cexit` running the `atexit` chain.
struct Exiting {
    /// The code the process ends with, when this run ends it.
    code: u32,
    /// Whether reaching the end of the chain terminates the guest (`exit`) or
    /// returns to it (`_cexit`).
    halt: bool,
    /// Handlers left to run, counted down from the end of the list.
    left: usize,
}

// ---- start-up -----------------------------------------------------------

/// `__set_app_type(type)`: 1 for a console program, 2 for a GUI one.
///
/// Nothing here behaves differently: the guest draws into the same surface and
/// gets the same standard handles either way.
fn set_app_type(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `__setusermatherr(handler)`: the function that reports a maths error.
///
/// This machine raises no floating-point exception the CRT would report, so the
/// handler is recorded by returning success and never called.
fn set_user_math_err(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `_adjust_fdiv()`: the one-time fix-up for the floating-point division sequence
/// an old compiler emitted. There is no sequence to adjust here, and the call is
/// a no-op on every Windows since NT 4.
fn adjust_fdiv(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_controlfp(new, mask)`: sets and reports the x87 control word.
fn controlfp(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(DEFAULT_CONTROL_WORD))
}

/// `__p__fmode()`: the address of `_fmode`.
fn p_fmode(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let mut cell = host.crt.fmode;
    let at = integer_cell(&mut cell, host, call)?;
    host.crt.fmode = cell;
    Ok(Return(at))
}

/// `__p__commode()`: the address of `_commode`.
fn p_commode(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let mut cell = host.crt.commode;
    let at = integer_cell(&mut cell, host, call)?;
    host.crt.commode = cell;
    Ok(Return(at))
}

/// The address of a four-byte cell, allocated on first use and zeroed then. A
/// later call hands back the same cell, so a value the guest wrote survives.
fn integer_cell(cell: &mut Option<u32>, host: &mut Host, call: &mut Call<'_>) -> Result<u32> {
    if cell.is_none() {
        let at = host.allocate(4, 4)?;
        call.write_u32(at, 0)?;
        *cell = Some(at);
    }
    Ok(cell.unwrap())
}

/// `__getmainargs(&argc, &argv, &envp, expand, startup)`.
///
/// The console-side arguments come from the command line the host gives the guest
/// (`GetCommandLineA`), split the way Windows splits one: whitespace between
/// arguments, double quotes grouping a single one. The environment is empty, so
/// `envp` points at a null pointer, which is what a program iterating it expects.
fn main_args(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let (argc, argv) = match host.crt.argv {
        Some(built) => built,
        None => {
            let line_pointer = host.command_line(call.memory)?;
            let line = cstr_bytes(call.memory, line_pointer)?;
            let words = split_command_line(&String::from_utf8_lossy(&line));
            // Each argument is its own string in guest memory, and the array is
            // null-terminated so a program that never trusts `argc` still stops.
            let mut pointers = Vec::with_capacity(words.len());
            for word in &words {
                let at = host.allocate(word.len() as u32 + 1, 4)?;
                call.write(at, word.as_bytes())?;
                call.write(at + word.len() as u32, &[0])?;
                pointers.push(at);
            }
            let array = host.allocate((pointers.len() as u32 + 1) * 4, 4)?;
            for (index, pointer) in pointers.iter().enumerate() {
                call.write_u32(array + index as u32 * 4, *pointer)?;
            }
            call.write_u32(array + pointers.len() as u32 * 4, 0)?;
            let built = (pointers.len() as u32, array);
            host.crt.argv = Some(built);
            built
        }
    };

    let argc_out = call.arg(0)?;
    if argc_out != 0 {
        call.write_u32(argc_out, argc)?;
    }
    let argv_out = call.arg(1)?;
    if argv_out != 0 {
        call.write_u32(argv_out, argv)?;
    }
    let env_out = call.arg(2)?;
    if env_out != 0 {
        let environ = match host.crt.environ {
            Some(environ) => environ,
            None => {
                let environ = host.environment(call.memory)?;
                host.crt.environ = Some(environ);
                environ
            }
        };
        call.write_u32(env_out, environ)?;
    }
    Ok(Return(0))
}

/// Splits a command line the way the Windows C runtime does: whitespace between
/// arguments, a pair of double quotes grouping one, and the quotes dropped.
fn split_command_line(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut started = false;
    for character in text.chars() {
        match character {
            '"' => {
                quoted = !quoted;
                started = true;
            }
            character if character.is_whitespace() && !quoted => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            character => {
                word.push(character);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// `_initterm(start, end)`: calls every function pointer in a link-time table.
fn initterm(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    walk_table(host, call, false)
}

/// `_initterm_e(start, end)`: the same walk, stopping at the first initialiser
/// that returns non-zero and returning that value.
fn initterm_e(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    walk_table(host, call, true)
}

/// One pass of a table walk: continue the walk if this is a resumption, otherwise
/// take the range from the arguments, then run the next non-null entry.
fn walk_table(host: &mut Host, call: &mut Call<'_>, failing: bool) -> Result<Action> {
    let resuming = std::mem::take(&mut host.resumed);
    let mut walk = match host.crt.walk.take() {
        Some(walk) => {
            if !resuming {
                return Err(
                    "_initterm was entered again while its previous walk was still running"
                        .to_string(),
                );
            }
            if failing && host.guest_result != 0 {
                return Ok(Return(std::mem::take(&mut host.guest_result)));
            }
            host.guest_result = 0;
            walk
        }
        None => {
            if resuming {
                return Err("_initterm was resumed with no walk in flight".to_string());
            }
            host.guest_result = 0;
            Walk {
                next: call.arg(0)?,
                end: call.arg(1)?,
            }
        }
    };

    // A null entry is a gap the linker left between initialisers.
    while walk.next < walk.end {
        let at = walk.next;
        walk.next = at + 4;
        let entry = call.memory.read_u32(at)?;
        if entry == 0 {
            continue;
        }
        host.crt.walk = Some(walk);
        host.guest_call = Some(GuestCall {
            entry,
            // An initialiser is `__cdecl` and takes no arguments.
            args: [0; 4],
            answer: None,
            resume: true,
        });
        return Ok(Return(0));
    }
    // The walk is done, so the import completes with no failure.
    host.guest_result = 0;
    Ok(Return(0))
}

// ---- process exit -------------------------------------------------------

/// `_onexit(handler)`: registers an exit handler and returns it, or 0 when the
/// list cannot take another.
fn onexit(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handler = call.arg(0)?;
    register(host, handler);
    Ok(Return(handler))
}

/// `atexit(handler)`: the same registration, with ANSI's 0 for success.
fn atexit(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    register(host, call.arg(0)?);
    Ok(Return(0))
}

/// `__dllonexit(handler, begin, end)`: the per-DLL exit table.
///
/// This machine runs the one list. A single guest and a single CRT make the
/// distinction between the process table and a DLL's table unobservable.
fn dllonexit(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handler = call.arg(0)?;
    register(host, handler);
    Ok(Return(handler))
}

/// Adds a handler to the exit chain. Registering one twice runs it twice, which
/// is what the CRT does: the list is a list, not a set.
fn register(host: &mut Host, handler: u32) {
    if handler != 0 {
        host.crt.atexit.push(handler);
    }
}

/// `exit(code)`: runs the exit handlers in reverse, then ends the process.
fn exit(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    run_atexit(host, true, call)
}

/// `_cexit()`: runs the exit handlers but returns to the caller.
fn cexit(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    run_atexit(host, false, call)
}

fn run_atexit(host: &mut Host, halt: bool, call: &mut Call<'_>) -> Result<Action> {
    let resuming = std::mem::take(&mut host.resumed);
    let mut exiting = match host.crt.exiting.take() {
        Some(exiting) if resuming => exiting,
        Some(_) => {
            return Err("exit was entered again while its handlers were still running".to_string())
        }
        None => {
            if resuming {
                return Err("exit was resumed with no handler run in flight".to_string());
            }
            Exiting {
                code: if halt { call.arg(0)? } else { 0 },
                halt,
                left: host.crt.atexit.len(),
            }
        }
    };

    while exiting.left > 0 {
        exiting.left -= 1;
        let handler = host.crt.atexit[exiting.left];
        if handler == 0 {
            continue;
        }
        host.crt.exiting = Some(exiting);
        host.guest_call = Some(GuestCall {
            entry: handler,
            args: [0; 4],
            answer: None,
            resume: true,
        });
        return Ok(Return(0));
    }

    host.crt.atexit.clear();
    if exiting.halt {
        Ok(Action::Exit(exiting.code))
    } else {
        Ok(Return(0))
    }
}

/// `_exit(code)`: ends the process without running the handlers.
fn exit_now(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Action::Exit(call.arg(0)?))
}

/// `abort()`: ends the process with a failure code, as the CRT does after it has
/// reported the reason.
fn abort(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.notice("the guest called abort()".to_string());
    Ok(Action::Exit(EXIT_FAILURE))
}

/// `_purecall()`: a virtual call reached a pure-virtual slot.
fn purecall(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.notice("the guest called a pure virtual function (_purecall)".to_string());
    Ok(Action::Exit(EXIT_FAILURE))
}

/// `std::terminate`, which the VC6 runtime exports as `?terminate@@YAXXZ`.
fn terminate(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.notice("the guest called std::terminate".to_string());
    Ok(Action::Exit(EXIT_FAILURE))
}

// ---- what the VC8 runtime calls -------------------------------------------

/// `_controlfp_s(current, new, mask)`: the same control word `_controlfp`
/// reports, in the shape VC8's secure variant uses -- an error code, 0 for
/// success, and the current word through the caller's pointer.
fn controlfp_s(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let current = call.arg(0)?;
    if current != 0 {
        call.write_u32(current, DEFAULT_CONTROL_WORD)?;
    }
    Ok(Return(0))
}

/// `_ismbblead(character)`: whether a byte can begin a multi-byte character.
///
/// The locale here is the C one, where a character is a byte, so no byte is a
/// lead byte. A guest that checks before splitting a string gets the right answer
/// for the strings this machine holds.
fn is_mbblead(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_crt_debugger_hook(condition)`: the debugger's report that a condition
/// failed. Nothing is attached to debug anything here, and the conditions are
/// checked by the shims themselves, so this is the no-op it is in a release
/// build.
fn crt_debugger_hook(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_XcptFilter(code, pointers)`: the CRT's own filter in a `__try`, which decides
/// whether the runtime handles an exception.
///
/// This machine reports a fault as its own stop -- the offending instruction and
/// the address -- rather than by unwinding into the guest's SEH frames, so the
/// filter's answer never decides anything: it says "keep searching", which is
/// what it says for a code it does not recognise.
fn xcpt_filter(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_except_handler4_common(...)`: the shared half of the guest's own
/// `__except_handler4`, which VC8 generated for every `__try` in the program.
///
/// It answers `ExceptionContinueSearch`: the same reason [`xcpt_filter`] does.
fn except_handler(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `__CxxFrameHandler3`: the C++ exception dispatcher VC8 compiles into a
/// function that can throw. Also unhandled here, and for the same reason.
fn cxx_frame_handler(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `_amsg_exit(number)`: the runtime's last report before it ends the process,
/// which is how a missing or unusable runtime component fails. 255 is the code
/// the real one exits with.
fn amsg_exit(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.notice(format!(
        "the guest's C runtime reported error R{} and ended the process (_amsg_exit)",
        6000 + call.arg(0)?
    ));
    Ok(Action::Exit(255))
}

/// `_invoke_watson(...)`: the invalid-parameter handler, which never returns. The
/// arguments are the expression, function and file it failed in, which would be
/// the message on a machine with that reporting turned on.
fn invoke_watson(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.notice("the guest's C runtime called the invalid-parameter handler".to_string());
    Ok(Action::Exit(EXIT_FAILURE))
}

// ---- housekeeping -------------------------------------------------------

/// `_lock(lock)` and `_unlock(lock)`: the CRT's own locks, which a single-threaded
/// machine never contends.
fn lock(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn unlock(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_configthreadlocale(kind)`: the previous per-thread locale setting. There is
/// one thread, and the locale is the C one.
fn config_thread_locale(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_encode_pointer(pointer)`: scrambles a pointer held across a callback.
///
/// This CRT keeps no cookie, so the round trip is the identity; both directions
/// agree, which is all the protocol requires.
fn encode_pointer(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?))
}

fn decode_pointer(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?))
}

// ---- heap ---------------------------------------------------------------

/// `malloc(size)`: a block from the same arena `HeapAlloc` and `VirtualAlloc` come
/// from, so a pointer allocated here can be released there and vice versa.
fn malloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.allocate(call.arg(0)?, 8)?))
}

/// `calloc(count, size)`: zeroed memory, because a reused arena block holds what
/// its last owner left.
fn calloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let count = call.arg(0)?;
    let size = call.arg(1)?;
    // Overflow is a failed allocation, not a smaller one.
    let Some(bytes) = count.checked_mul(size) else {
        return Ok(Return(0));
    };
    let at = host.allocate(bytes, 8)?;
    call.zero(at, bytes)?;
    Ok(Return(at))
}

/// `realloc(pointer, size)`.
fn realloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let pointer = call.arg(0)?;
    let size = call.arg(1)?;
    if pointer == 0 {
        return Ok(Return(host.allocate(size, 8)?));
    }
    if size == 0 {
        host.release(pointer);
        return Ok(Return(0));
    }
    Ok(Return(host.reallocate(call.memory, pointer, size, 8)?))
}

/// `free(pointer)`: the block goes back to the arena, which is what lets the
/// game's per-frame churn reuse memory instead of walking to its ceiling.
fn free(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let pointer = call.arg(0)?;
    if pointer != 0 {
        host.release(pointer);
    }
    Ok(Return(0))
}

/// `operator new` and `operator new[]`: both take a size and return a block.
fn allocate_object(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.allocate(call.arg(0)?, 8)?))
}

/// `operator delete` and `operator delete[]`: both take a block and release it.
fn free_object(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let pointer = call.arg(0)?;
    if pointer != 0 {
        host.release(pointer);
    }
    Ok(Return(0))
}

// ---- stdio --------------------------------------------------------------

/// MSVC's `FILE`, as the guest sees it: 32 bytes of `struct _iobuf`.
///
/// A program compiled against the VC headers does not call `feof`: the macro reads
/// `_flag` at offset 12 of the block. So the block has to be the runtime's layout
/// with the runtime's bit values, not an opaque pointer into host state.
const FILE_BYTES: u32 = 32;
const FILE_FLAG: u32 = 12;
const FILE_DESCRIPTOR: u32 = 16;

/// `_IOREAD` and `_IOWRT`: the mode the stream was opened with.
const IO_READ: u32 = 0x0001;
const IO_WRITE: u32 = 0x0002;
/// `_IOEOF`, which is the bit `feof` tests. A read that comes up short sets it.
const IO_EOF: u32 = 0x0010;
/// `_IOERR`, which is the bit `ferror` tests.
const IO_ERROR: u32 = 0x0020;

/// `EOF`, which a failed `fclose` and a failed read report.
const EOF: u32 = 0xffff_ffff;

/// How a mode string asks a file to be opened.
struct OpenMode {
    readable: bool,
    writable: bool,
    append: bool,
    /// Create the file when it is not there.
    create: bool,
    /// Empty it when it is.
    truncate: bool,
}

/// `"r"`, `"wb"`, `"a+"` and the rest, as the CRT reads them.
fn parse_mode(mode: &str) -> Option<OpenMode> {
    let mut characters = mode.chars();
    let mut open = OpenMode {
        readable: false,
        writable: false,
        append: false,
        create: false,
        truncate: false,
    };
    match characters.next()?.to_ascii_lowercase() {
        'r' => open.readable = true,
        'w' => {
            open.writable = true;
            open.create = true;
            open.truncate = true;
        }
        'a' => {
            open.writable = true;
            open.create = true;
            open.append = true;
        }
        _ => return None,
    }
    for character in characters {
        match character.to_ascii_lowercase() {
            '+' => {
                open.readable = true;
                open.writable = true;
            }
            // Text and binary are one file: no line-ending translation exists
            // here, and a guest that asks for either gets the bytes it wrote.
            'b' | 't' => {}
            _ => return None,
        }
    }
    Some(open)
}

/// `fopen(path, mode)`.
fn fopen(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const CREATE_NEW: u32 = 1;
    const CREATE_ALWAYS: u32 = 2;
    const OPEN_EXISTING: u32 = 3;

    let path = String::from_utf8_lossy(&cstr_bytes(call.memory, call.arg(0)?)?).into_owned();
    let mode = String::from_utf8_lossy(&cstr_bytes(call.memory, call.arg(1)?)?).into_owned();
    // The caller's buffer for a mode string is not ours to free, and offering
    // the `_wfopen`/`fopen_s` shapes is a different signature, so a mode this
    // host cannot read is simply a failed open.
    let Some(request) = parse_mode(&mode) else {
        return Ok(Return(0));
    };

    let handle = if request.truncate {
        host.files.create(&path, CREATE_ALWAYS)
    } else if request.writable {
        host.files
            .create(&path, OPEN_EXISTING)
            .or_else(|| request.create.then(|| host.files.create(&path, CREATE_NEW)).flatten())
    } else {
        host.files.open(&path)
    };
    if std::env::var("SHIMKIT_LOG_READS").is_ok() && host.notices.len() < 400 {
        host.notice(format!("fopen \"{path}\" mode \"{mode}\" -> {}", handle.is_some()));
    }
    let Some(handle) = handle else {
        return Ok(Return(0));
    };
    if request.append {
        let end = host.files.length(handle).unwrap_or(0);
        host.files.seek(handle, end);
    }

    let file = host.allocate(FILE_BYTES, 4)?;
    call.zero(file, FILE_BYTES)?;
    let descriptor = host.crt.next_stream;
    host.crt.next_stream += 1;
    let mut flags = 0;
    if request.readable {
        flags |= IO_READ;
    }
    if request.writable {
        flags |= IO_WRITE;
    }
    call.write_u32(file + FILE_FLAG, flags)?;
    call.write_u32(file + FILE_DESCRIPTOR, descriptor)?;
    host.crt.streams.insert(
        file,
        Stream {
            handle,
            readable: request.readable,
            writable: request.writable,
            append: request.append,
        },
    );
    Ok(Return(file))
}

/// `fclose(stream)`: closes the file and gives the `FILE` block back.
fn fclose(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    let Some(stream) = host.crt.streams.remove(&file) else {
        return Ok(Return(EOF));
    };
    host.files.close(stream.handle);
    host.release(file);
    Ok(Return(0))
}

/// `fread(buffer, size, count, stream)`: returns how many whole items arrived.
fn fread(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let size = call.arg(1)?;
    let count = call.arg(2)?;
    let file = call.arg(3)?;
    if size == 0 {
        return Ok(Return(0));
    }
    let Some(requested) = size.checked_mul(count) else {
        return Ok(Return(0));
    };
    let Some((handle, readable)) = host
        .crt
        .streams
        .get(&file)
        .map(|stream| (stream.handle, stream.readable))
    else {
        return Ok(Return(0));
    };
    if !readable {
        set_flag(call, file, IO_ERROR)?;
        return Ok(Return(0));
    }
    let bytes = host.files.read(handle, requested).unwrap_or_default();
    call.write(destination, &bytes)?;
    // A short read means the file ended, which is what the guest's own `feof`
    // macro will look for in the block.
    if (bytes.len() as u32) < requested {
        set_flag(call, file, IO_EOF)?;
    }
    Ok(Return(bytes.len() as u32 / size))
}

/// `fwrite(buffer, size, count, stream)`: returns how many whole items went out.
fn fwrite(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let source = call.arg(0)?;
    let size = call.arg(1)?;
    let count = call.arg(2)?;
    let file = call.arg(3)?;
    if size == 0 {
        return Ok(Return(0));
    }
    let Some(requested) = size.checked_mul(count) else {
        return Ok(Return(0));
    };
    let bytes = call.memory.read_bytes(source, requested as usize)?.to_vec();
    let written = match write_stream(host, call, file, &bytes)? {
        Return(written) => written,
        _ => return Ok(Return(0)),
    };
    Ok(Return(written / size))
}

/// The write half of a stream, shared by `fwrite` and `fprintf`.
fn write_stream(
    host: &mut Host,
    call: &mut Call<'_>,
    file: u32,
    bytes: &[u8],
) -> Result<Action> {
    let Some((handle, writable, append)) = host
        .crt
        .streams
        .get(&file)
        .map(|stream| (stream.handle, stream.writable, stream.append))
    else {
        return Ok(Return(EOF));
    };
    if !writable {
        set_flag(call, file, IO_ERROR)?;
        return Ok(Return(EOF));
    }
    if append {
        let end = host.files.length(handle).unwrap_or(0);
        host.files.seek(handle, end);
    }
    match host.files.write(handle, bytes) {
        Some(written) => Ok(Return(written)),
        None => {
            set_flag(call, file, IO_ERROR)?;
            Ok(Return(EOF))
        }
    }
}

/// `fseek(stream, offset, origin)`: 0 when the cursor moved, -1 when it did not.
fn fseek(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    let offset = i64::from(call.arg(1)? as i32);
    let origin = call.arg(2)?;
    let Some(handle) = host.crt.streams.get(&file).map(|stream| stream.handle) else {
        return Ok(Return(EOF));
    };
    let position = host.files.position(handle).unwrap_or(0);
    let length = host.files.length(handle).unwrap_or(0);
    let base = match origin {
        0 => 0,                       // SEEK_SET
        1 => i64::from(position),     // SEEK_CUR
        2 => i64::from(length),       // SEEK_END
        _ => return Ok(Return(EOF)),
    };
    let target = base + offset;
    if !(0..=i64::from(u32::MAX)).contains(&target) {
        return Ok(Return(EOF));
    }
    host.files.seek(handle, target as u32);
    // A seek clears the end-of-file state, which is what the CRT does: the
    // cursor may be back inside the file.
    clear_flag(call, file, IO_EOF)?;
    Ok(Return(0))
}

/// `ftell(stream)`: where the cursor is.
fn ftell(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    let Some(handle) = host.crt.streams.get(&file).map(|stream| stream.handle) else {
        return Ok(Return(EOF));
    };
    match host.files.position(handle) {
        Some(position) if position <= i32::MAX as u32 => Ok(Return(position)),
        _ => Ok(Return(EOF)),
    }
}

/// `feof(stream)`: the end-of-file bit, which is also what the macro reads.
fn feof(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    Ok(Return(u32::from(flag_set(call, file, IO_EOF)?)))
}

/// `ferror(stream)`.
fn ferror(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    Ok(Return(u32::from(flag_set(call, file, IO_ERROR)?)))
}

/// `fgets(buffer, size, stream)`: one line, its newline included, terminated.
///
/// It answers 0 when the stream is at its end and nothing was read -- a line
/// without its newline, or with one, is still a line.
fn fgets(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let buffer = call.arg(0)?;
    let size = call.arg(1)? as i32;
    let file = call.arg(2)?;
    if size <= 0 {
        return Ok(Return(0));
    }
    let Some((handle, readable)) = host
        .crt
        .streams
        .get(&file)
        .map(|stream| (stream.handle, stream.readable))
    else {
        return Ok(Return(0));
    };
    if !readable {
        set_flag(call, file, IO_ERROR)?;
        return Ok(Return(0));
    }
    let room = size as usize - 1;
    let mut text = Vec::new();
    let mut ended = false;
    while text.len() < room {
        match host.files.read(handle, 1) {
            Some(bytes) if !bytes.is_empty() => {
                text.push(bytes[0]);
                if bytes[0] == b'\n' {
                    break;
                }
            }
            _ => {
                ended = true;
                break;
            }
        }
    }
    if text.is_empty() {
        // Nothing to hand back: the end of the file is the only reason a stream
        // with room left reads nothing at all.
        if ended {
            set_flag(call, file, IO_EOF)?;
        }
        return Ok(Return(0));
    }
    text.push(0);
    call.write(buffer, &text)?;
    if ended {
        set_flag(call, file, IO_EOF)?;
    }
    Ok(Return(buffer))
}

fn flag_set(call: &Call<'_>, file: u32, flag: u32) -> Result<bool> {
    if file == 0 {
        return Ok(false);
    }
    Ok(call.memory.read_u32(file + FILE_FLAG)? & flag != 0)
}

fn set_flag(call: &mut Call<'_>, file: u32, flag: u32) -> Result<()> {
    if file == 0 {
        return Ok(());
    }
    let flags = call.memory.read_u32(file + FILE_FLAG)?;
    call.write_u32(file + FILE_FLAG, flags | flag)
}

fn clear_flag(call: &mut Call<'_>, file: u32, flag: u32) -> Result<()> {
    if file == 0 {
        return Ok(());
    }
    let flags = call.memory.read_u32(file + FILE_FLAG)?;
    call.write_u32(file + FILE_FLAG, flags & !flag)
}

// ---- formatted output ----------------------------------------------------

/// `sprintf(destination, format, ...)`.
fn sprintf(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let format = cstr_bytes(call.memory, call.arg(1)?)?;
    // The variadic arguments start after the two the signature names, which is
    // where the caller pushed them.
    let text = format::render(call.memory, &format, call.args + 8)?;
    call.write(destination, &text)?;
    call.write(destination + text.len() as u32, &[0])?;
    Ok(Return(text.len() as u32))
}

/// `vsprintf(destination, format, arguments)`: the same with a `va_list`,
/// which on this calling convention is a pointer to the next argument.
fn vsprintf(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let format = cstr_bytes(call.memory, call.arg(1)?)?;
    let text = format::render(call.memory, &format, call.arg(2)?)?;
    call.write(destination, &text)?;
    call.write(destination + text.len() as u32, &[0])?;
    Ok(Return(text.len() as u32))
}

/// `fprintf(stream, format, ...)`.
fn fprintf(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    let format = cstr_bytes(call.memory, call.arg(1)?)?;
    let text = format::render(call.memory, &format, call.args + 8)?;
    write_stream(host, call, file, &text)
}

// ---- formatted input -----------------------------------------------------

/// How much of a stream one `fscanf` call reads at a time. A conversion is taken
/// from this window and the stream is put back where the conversion stopped, so
/// the next call continues from there; a token longer than the window is the one
/// shape a fixed window cannot serve.
const SCAN_WINDOW: u32 = 8192;

/// `sscanf(source, format, ...)`.
fn sscanf(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let format = cstr_bytes(call.memory, call.arg(1)?)?;
    let input = cstr_bytes(call.memory, call.arg(0)?)?;
    let mut next = call.args + 8;
    let (converted, _) = format::scan(call.memory, &format, &input, &mut next)?;
    Ok(Return(converted))
}

/// `fscanf(stream, format, ...)`.
fn fscanf(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let file = call.arg(0)?;
    let format = cstr_bytes(call.memory, call.arg(1)?)?;
    let Some((handle, readable)) = host
        .crt
        .streams
        .get(&file)
        .map(|stream| (stream.handle, stream.readable))
    else {
        return Ok(Return(EOF));
    };
    if !readable {
        set_flag(call, file, IO_ERROR)?;
        return Ok(Return(EOF));
    }
    let start = host.files.position(handle).unwrap_or(0);
    let length = host.files.length(handle).unwrap_or(0);
    let input = host
        .files
        .read(handle, SCAN_WINDOW.min(length.saturating_sub(start)))
        .unwrap_or_default();
    if input.is_empty() {
        set_flag(call, file, IO_EOF)?;
        return Ok(Return(EOF));
    }
    let mut next = call.args + 8;
    let (converted, consumed) = format::scan(call.memory, &format, &input, &mut next)?;
    // Whatever the conversions did not take stays in the stream, so a `getc` after
    // a `%d` sees the character that ended the number.
    host.files.seek(handle, start.wrapping_add(consumed as u32));
    if converted == 0 {
        // Nothing was assigned. C tells two cases apart: running out of input
        // before the first conversion is `EOF`, while a character that does not
        // match the format is a count of zero with that character still there.
        if consumed == input.len() {
            set_flag(call, file, IO_EOF)?;
            return Ok(Return(EOF));
        }
        return Ok(Return(0));
    }
    Ok(Return(converted))
}

// ---- the working directory ------------------------------------------------

/// `_getcwd(buffer, size)`: the bundle, which is also what
/// `GetCurrentDirectoryA` answers, because every name the guest uses resolves
/// inside it.
fn getcwd(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let buffer = call.arg(0)?;
    let size = call.arg(1)? as i32;
    if buffer == 0 || size < 2 {
        return Ok(Return(0));
    }
    call.write(buffer, b".\0")?;
    Ok(Return(buffer))
}

/// `_chdir(path)`: there is one directory, so a change of directory succeeds and
/// changes nothing; `_getcwd` keeps answering it.
fn chdir(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

// ---- the clock ------------------------------------------------------------

/// `_time64(out)`: the fixed instant, written through `out` when the caller gave
/// one and answered in `edx:eax` either way.
fn time64(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    if at != 0 {
        call.write_u32(at, FIXED_SECONDS as u32)?;
        call.write_u32(at.wrapping_add(4), 0)?;
    }
    Ok(Action::Return64(FIXED_SECONDS as u64))
}

/// `_localtime64(seconds)`: the fixed instant broken into `struct tm`.
///
/// The argument is not consulted: this machine's clock does not move, and
/// [`time64`] is the only source of a `time_t` here, so re-deriving the answer
/// from a value the host never produced would make it depend on whatever was in
/// the caller's variable.
fn localtime64(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = match host.crt.tm {
        Some(at) => at,
        None => {
            let at = host.allocate(TM_BYTES, 4)?;
            call.zero(at, TM_BYTES)?;
            host.crt.tm = Some(at);
            at
        }
    };
    // 2000-01-01 12:00:00, a Saturday and the first day of the year, with no
    // daylight saving: the same fields `GetLocalTime` reports.
    for (index, value) in [0_u32, 0, 12, 1, 0, 100, 6, 0, 0].iter().enumerate() {
        call.write_u32(at + index as u32 * 4, *value)?;
    }
    Ok(Return(at))
}

// ---- memory and strings -------------------------------------------------

/// `memset(destination, value, count)`.
fn memset(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let value = call.arg(1)? as u8;
    let count = call.arg(2)?;
    fill(call, destination, value, count)?;
    Ok(Return(destination))
}

/// Writes `value` `count` times, in chunks rather than a buffer the size of the
/// request: a game clears a whole surface through here.
fn fill(call: &mut Call<'_>, destination: u32, value: u8, count: u32) -> Result<()> {
    const CHUNK: u32 = 4096;
    let bytes = [value; CHUNK as usize];
    let mut at = destination;
    let mut left = count;
    while left > 0 {
        let size = left.min(CHUNK);
        call.write(at, &bytes[..size as usize])?;
        at = at.wrapping_add(size);
        left -= size;
    }
    Ok(())
}

/// `memcpy(destination, source, count)`: the ranges are not allowed to overlap,
/// and copying them in order answers an overlapping one the same way glibc does.
fn memcpy(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let source = call.arg(1)?;
    let count = call.arg(2)?;
    call.memory.copy(source, destination, count as usize)?;
    Ok(Return(destination))
}

/// `memmove(destination, source, count)`: the same copy, with overlap defined.
fn memmove(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let source = call.arg(1)?;
    let count = call.arg(2)?;
    call.memory.copy(source, destination, count as usize)?;
    Ok(Return(destination))
}

/// `memcmp(left, right, count)`: negative, zero or positive by the first byte
/// that differs.
fn memcmp(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let left = call.arg(0)?;
    let right = call.arg(1)?;
    let count = call.arg(2)?;
    let mut at = 0_u32;
    while at < count {
        let size = (count - at).min(4096);
        let one = call.memory.read_bytes(left + at, size as usize)?;
        let other = call.memory.read_bytes(right + at, size as usize)?;
        for index in 0..size as usize {
            if one[index] != other[index] {
                let difference = i32::from(one[index]) - i32::from(other[index]);
                return Ok(Return(difference as u32));
            }
        }
        at += size;
    }
    Ok(Return(0))
}

/// `strlen(string)`.
fn strlen(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    let bytes = cstr_bytes(call.memory, at)?;
    Ok(Return(bytes.len() as u32))
}

/// `strcpy(destination, source)`: the string and its terminator.
fn strcpy(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let bytes = cstr_bytes(call.memory, call.arg(1)?)?;
    call.write(destination, &bytes)?;
    call.write(destination + bytes.len() as u32, &[0])?;
    Ok(Return(destination))
}

/// `strncpy(destination, source, count)`: at most `count` bytes, zero-padded to
/// `count` when the source is shorter.
fn strncpy(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let count = call.arg(2)?;
    let mut bytes = cstr_bytes(call.memory, call.arg(1)?)?;
    bytes.truncate(count as usize);
    call.write(destination, &bytes)?;
    let padding = count - bytes.len() as u32;
    if padding > 0 {
        fill(call, destination + bytes.len() as u32, 0, padding)?;
    }
    Ok(Return(destination))
}

/// `strcat(destination, source)`.
fn strcat(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    let suffix = cstr_bytes(call.memory, call.arg(1)?)?;
    let prefix = cstr_bytes(call.memory, destination)?;
    let at = destination + prefix.len() as u32;
    call.write(at, &suffix)?;
    call.write(at + suffix.len() as u32, &[0])?;
    Ok(Return(destination))
}

/// `strcmp(left, right)`.
fn strcmp(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    compare(call, call.arg(0)?, call.arg(1)?, u32::MAX)
}

/// `strncmp(left, right, count)`.
fn strncmp(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let count = call.arg(2)?;
    compare(call, call.arg(0)?, call.arg(1)?, count)
}

/// Compares two strings byte by byte, up to `count` bytes, and answers the first
/// difference as the CRT does. The comparison is on bytes, not on a decoded
/// `String`: a game's text need not be UTF-8.
fn compare(call: &Call<'_>, left: u32, right: u32, count: u32) -> Result<Action> {
    let mut at = 0_u32;
    while at < count {
        let one = call.memory.read_u8(left.wrapping_add(at))?;
        let other = call.memory.read_u8(right.wrapping_add(at))?;
        if one != other {
            return Ok(Return((i32::from(one) - i32::from(other)) as u32));
        }
        if one == 0 {
            break;
        }
        at += 1;
    }
    Ok(Return(0))
}

/// `strchr(string, character)`: the first occurrence, or the terminator when the
/// character searched for is `'\0'`.
fn strchr(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let at = call.arg(0)?;
    let wanted = call.arg(1)? as u8;
    let bytes = cstr_bytes(call.memory, at)?;
    let mut index = 0_usize;
    while index < bytes.len() {
        if bytes[index] == wanted {
            return Ok(Return(at + index as u32));
        }
        index += 1;
    }
    if wanted == 0 {
        return Ok(Return(at + bytes.len() as u32));
    }
    Ok(Return(0))
}

/// `strstr(haystack, needle)`: the first occurrence of a substring, or 0.
fn strstr(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let haystack_at = call.arg(0)?;
    let haystack = cstr_bytes(call.memory, haystack_at)?;
    let needle = cstr_bytes(call.memory, call.arg(1)?)?;
    if needle.is_empty() {
        return Ok(Return(haystack_at));
    }
    if needle.len() > haystack.len() {
        return Ok(Return(0));
    }
    match haystack
        .windows(needle.len())
        .position(|window| window == needle.as_slice())
    {
        Some(index) => Ok(Return(haystack_at + index as u32)),
        None => Ok(Return(0)),
    }
}

/// The bytes of a NUL-terminated string, recorded exactly.
///
/// [`GuestMemory::read_cstr`] decodes to a `String`, which is the wrong shape for
/// a length or a copy: a byte above ASCII would be replaced and change both.
pub(crate) fn cstr_bytes(memory: &GuestMemory, at: u32) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for offset in 0..0x10_000u32 {
        let byte = memory.read_u8(at.wrapping_add(offset))?;
        if byte == 0 {
            return Ok(bytes);
        }
        bytes.push(byte);
    }
    Err(format!("string at {at:#010x} is not terminated"))
}

// ---- the rest -----------------------------------------------------------

/// `rand()`: MSVC's linear congruential generator, so a seed a game chooses
/// produces the sequence it was written against.
fn rand(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.crt.seed = host.crt.seed.wrapping_mul(214_013).wrapping_add(2_531_011);
    Ok(Return((host.crt.seed >> 16) & 0x7fff))
}

/// `srand(seed)`.
fn srand(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.crt.seed = call.arg(0)?;
    Ok(Return(0))
}

/// `sqrt(value)` and the rest: a `double` argument on the stack, the result on
/// the float stack.
fn sqrt(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::sqrt)
}

fn sin(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::sin)
}

fn cos(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::cos)
}

fn atan(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::atan)
}

fn asin(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::asin)
}

fn fabs(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::abs)
}

fn floor(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    push_double(call, f64::floor)
}

/// `pow(base, exponent)`: two doubles on the stack, unlike `_CIpow`, whose
/// arguments are already on the float stack.
fn pow(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let base = double_at(call, 0)?;
    let exponent = double_at(call, 2)?;
    Ok(Action::Float(crate::cpu::FloatOp::Push(
        base.powf(exponent).to_bits(),
    )))
}

/// Reads the `double` at argument `index` and asks for the computed value to be
/// pushed onto the float stack.
fn push_double(call: &mut Call<'_>, function: fn(f64) -> f64) -> Result<Action> {
    let value = double_at(call, 0)?;
    Ok(Action::Float(crate::cpu::FloatOp::Push(
        function(value).to_bits(),
    )))
}

/// The `double` at argument `index`: two dwords, low first.
fn double_at(call: &Call<'_>, index: u32) -> Result<f64> {
    let low = u64::from(call.arg(index)?);
    let high = u64::from(call.arg(index + 1)?);
    Ok(f64::from_bits((high << 32) | low))
}

/// `_CIacos(value)`: the argument is already on the float stack, and so is the
/// answer.
fn ci_acos(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Action::Float(crate::cpu::FloatOp::Unary(
        crate::cpu::Unary::Acos,
    )))
}

/// `_CIpow(base, exponent)`: both on the float stack, exponent on top.
fn ci_pow(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Action::Float(crate::cpu::FloatOp::Pow))
}

/// `_ftol()`: truncate the top of the float stack into `edx:eax`.
fn ftol(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Action::Float(crate::cpu::FloatOp::TruncateToInt))
}

/// `_finite(value)`: non-zero when a double is neither infinite nor a NaN.
fn finite(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The exponent field is the top eleven bits of the high word.
    let high = call.arg(1)?;
    Ok(Return(u32::from(high & 0x7ff0_0000 != 0x7ff0_0000)))
}

/// Fills the IAT slots that hold a CRT *variable* rather than a function.
///
/// `_acmdln` is declared `char *_acmdln`, so the import gives the address of a
/// pointer, not a routine: the guest loads the slot, then dereferences it. Left
/// as an import stub, that load would hand the guest a code address and the
/// first `printf` of the command line would print it.
pub fn fill_data_slots(
    host: &mut Host,
    memory: &mut GuestMemory,
    table: crate::dispatch::Table,
) -> Result<()> {
    for slot in table.slots.iter() {
        if !crate::shim::is_c_runtime(&slot.dll) {
            continue;
        }
        if slot.name == "_acmdln" {
            let variable = match host.crt.acmdln {
                Some(variable) => variable,
                None => {
                    let variable = host.allocate(4, 4)?;
                    host.crt.acmdln = Some(variable);
                    variable
                }
            };
            let line = host.command_line(memory)?;
            memory.write_u32(variable, line)?;
            memory.write_import(slot.iat_va, variable)?;
        }
    }
    Ok(())
}

/// The `printf` conversions, applied to guest memory.
///
/// A variadic import has no signature to read: the shim knows where the first
/// argument is, and every conversion consumes whole dwords from there. The
/// conversions are the ones VC6's own `printf` accepts, because that is what the
/// program was compiled against.
mod format {
    use crate::memory::GuestMemory;
    use crate::Result;

    /// The flags, width and precision one conversion carries.
    #[derive(Default)]
    struct Spec {
        left: bool,
        plus: bool,
        space: bool,
        alternate: bool,
        zero: bool,
        width: Option<usize>,
        precision: Option<usize>,
        length: Length,
    }

    #[derive(Clone, Copy, Default, PartialEq, Eq)]
    enum Length {
        /// No modifier: an `int`.
        #[default]
        Word,
        /// `h`: the argument is a short.
        Short,
        /// `l` and `L`. A long is a dword here, and a long double is read as a
        /// double because that is what the FPU keeps.
        Long,
        /// MSVC's `I64`: two dwords.
        Wide,
    }

    /// Renders `format` with the arguments starting at the guest address `next`,
    /// and answers the bytes to write.
    pub fn render(memory: &GuestMemory, format: &[u8], mut next: u32) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut index = 0;
        while index < format.len() {
            let byte = format[index];
            let percent = index;
            index += 1;
            if byte != b'%' {
                out.push(byte);
                continue;
            }
            let Some(&leading) = format.get(index) else {
                break;
            };
            if leading == b'%' {
                out.push(b'%');
                index += 1;
                continue;
            }

            let mut spec = Spec::default();
            while let Some(&flag) = format.get(index) {
                match flag {
                    b'-' => spec.left = true,
                    b'+' => spec.plus = true,
                    b' ' => spec.space = true,
                    b'#' => spec.alternate = true,
                    b'0' => spec.zero = true,
                    _ => break,
                }
                index += 1;
            }

            if format.get(index) == Some(&b'*') {
                let width = read(memory, &mut next)? as i32;
                if width < 0 {
                    spec.left = true;
                    spec.width = Some(width.unsigned_abs() as usize);
                } else {
                    spec.width = Some(width as usize);
                }
                index += 1;
            } else {
                let (width, moved) = digits(format, index);
                spec.width = width;
                index += moved;
            }

            if format.get(index) == Some(&b'.') {
                index += 1;
                if format.get(index) == Some(&b'*') {
                    let precision = read(memory, &mut next)? as i32;
                    spec.precision = Some(precision.max(0) as usize);
                    index += 1;
                } else {
                    let (precision, moved) = digits(format, index);
                    spec.precision = Some(precision.unwrap_or(0));
                    index += moved;
                }
            }

            match format.get(index) {
                Some(b'h') => {
                    spec.length = Length::Short;
                    index += 1;
                }
                Some(b'l') | Some(b'L') => {
                    spec.length = Length::Long;
                    index += 1;
                }
                Some(b'I')
                    if format.get(index + 1) == Some(&b'6') && format.get(index + 2) == Some(&b'4') =>
                {
                    spec.length = Length::Wide;
                    index += 3;
                }
                _ => {}
            }

            let Some(&conversion) = format.get(index) else {
                break;
            };
            index += 1;
            // A precision turns the `0` flag off for integers, which is C's rule.
            let zeros = spec.zero && spec.precision.is_none();
            match conversion {
                b'd' | b'i' => {
                    let value = signed(memory, &mut next, spec.length)?;
                    let digits = with_precision(value.unsigned_abs().to_string(), &spec, value == 0);
                    let sign = if value < 0 {
                        "-"
                    } else if spec.plus {
                        "+"
                    } else if spec.space {
                        " "
                    } else {
                        ""
                    };
                    let text = format!("{sign}{digits}").into_bytes();
                    out.extend(justified(text, sign.len(), &spec, zeros));
                }
                b'u' => {
                    let value = unsigned(memory, &mut next, spec.length)?;
                    let digits = with_precision(value.to_string(), &spec, value == 0);
                    out.extend(justified(digits.into_bytes(), 0, &spec, zeros));
                }
                b'x' | b'X' => {
                    let value = unsigned(memory, &mut next, spec.length)?;
                    let upper = conversion == b'X';
                    let digits = if upper {
                        format!("{value:X}")
                    } else {
                        format!("{value:x}")
                    };
                    let digits = with_precision(digits, &spec, value == 0);
                    let prefix = if spec.alternate && value != 0 {
                        if upper { "0X" } else { "0x" }
                    } else {
                        ""
                    };
                    let text = format!("{prefix}{digits}").into_bytes();
                    out.extend(justified(text, prefix.len(), &spec, zeros));
                }
                b'o' => {
                    let value = unsigned(memory, &mut next, spec.length)?;
                    let digits = with_precision(format!("{value:o}"), &spec, value == 0);
                    // In octal the `#` prefix is one more leading zero.
                    let digits = if spec.alternate && !digits.starts_with('0') {
                        format!("0{digits}")
                    } else {
                        digits
                    };
                    out.extend(justified(digits.into_bytes(), 0, &spec, zeros));
                }
                b'c' => {
                    let character = read(memory, &mut next)? as u8;
                    out.extend(justified(vec![character], 0, &spec, false));
                }
                b's' => {
                    let pointer = read(memory, &mut next)?;
                    let mut text = if pointer == 0 {
                        b"(null)".to_vec()
                    } else {
                        super::cstr_bytes(memory, pointer)?
                    };
                    if let Some(precision) = spec.precision {
                        text.truncate(precision);
                    }
                    out.extend(justified(text, 0, &spec, false));
                }
                b'p' => {
                    let pointer = read(memory, &mut next)?;
                    out.extend(justified(format!("{pointer:08X}").into_bytes(), 0, &spec, zeros));
                }
                b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                    let value = double(memory, &mut next)?;
                    let precision = spec.precision.unwrap_or(6);
                    let text = match conversion {
                        b'f' | b'F' => fixed(value, precision),
                        b'e' | b'E' => scientific(value, precision, conversion == b'E'),
                        _ => general(value, precision, conversion == b'G'),
                    };
                    let lead = usize::from(matches!(text.first(), Some(b'-') | Some(b'+')));
                    out.extend(justified(text, lead, &spec, spec.zero));
                }
                _ => {
                    // A conversion this host does not implement is echoed whole,
                    // never dropped: the caller sees the field it asked for in the
                    // output instead of a silently missing one.
                    out.extend_from_slice(&format[percent..index]);
                }
            }
        }
        Ok(out)
    }

    /// The `scanf` conversions, applied to input the caller has already read.
    ///
    /// Answers how many items were assigned and how much of the input was used. A
    /// stream puts its cursor back by that much, which is what leaves the next
    /// call the character that ended the last conversion -- a space, a comma the
    /// format did not ask for, the letter after a number.
    ///
    /// A conversion this host does not implement stops the scan where it stands,
    /// so the count already assigned is the answer. That is the one thing worth
    /// being strict about: writing a value the format did not ask for would be
    /// worse than a short count, because the caller checks the count and cannot
    /// check the value.
    pub fn scan(
        memory: &mut GuestMemory,
        format: &[u8],
        input: &[u8],
        next: &mut u32,
    ) -> Result<(u32, usize)> {
        let mut at = 0usize;
        let mut converted = 0u32;
        let mut index = 0usize;
        while index < format.len() {
            let byte = format[index];
            if byte.is_ascii_whitespace() {
                // A whitespace run in the format eats any run in the input, which
                // is why `"%d %d"` and `"%d%d"` read the same two numbers.
                while format.get(index).is_some_and(|byte| byte.is_ascii_whitespace()) {
                    index += 1;
                }
                skip_whitespace(input, &mut at);
                continue;
            }
            if byte != b'%' {
                // Any other character has to be there, and is not counted as a
                // conversion.
                if input.get(at) != Some(&byte) {
                    return Ok((converted, at));
                }
                at += 1;
                index += 1;
                continue;
            }
            index += 1;
            if format.get(index) == Some(&b'%') {
                if input.get(at) != Some(&b'%') {
                    return Ok((converted, at));
                }
                at += 1;
                index += 1;
                continue;
            }

            let (width, moved) = digits(format, index);
            index += moved;
            let mut length = Length::Word;
            match format.get(index) {
                Some(b'h') => {
                    length = Length::Short;
                    index += 1;
                }
                Some(b'l') | Some(b'L') => {
                    length = Length::Long;
                    index += 1;
                }
                Some(b'I')
                    if format.get(index + 1) == Some(&b'6') && format.get(index + 2) == Some(&b'4') =>
                {
                    length = Length::Wide;
                    index += 3;
                }
                _ => {}
            }
            let Some(&conversion) = format.get(index) else {
                break;
            };
            index += 1;
            match conversion {
                b'd' | b'i' | b'u' | b'o' | b'x' | b'X' => {
                    skip_whitespace(input, &mut at);
                    let end = stop(input, at, width);
                    let radix = match conversion {
                        b'o' => 8,
                        b'x' | b'X' => 16,
                        _ => 10,
                    };
                    let Some((value, used)) = number(&input[at..end], radix, conversion == b'i')
                    else {
                        return Ok((converted, at));
                    };
                    at += used;
                    store(memory, next, length, value)?;
                    converted += 1;
                }
                b'c' => {
                    // No whitespace is skipped and no terminator is written: a
                    // field of `width` characters, one by default.
                    let count = width.unwrap_or(1);
                    if input.len() - at < count {
                        return Ok((converted, at));
                    }
                    let pointer = read(memory, next)?;
                    memory.write_bytes(pointer, &input[at..at + count])?;
                    at += count;
                    converted += 1;
                }
                b's' => {
                    skip_whitespace(input, &mut at);
                    let end = stop(input, at, width);
                    if end == at {
                        return Ok((converted, at));
                    }
                    let pointer = read(memory, next)?;
                    memory.write_bytes(pointer, &input[at..end])?;
                    memory.write_u8(pointer + (end - at) as u32, 0)?;
                    at = end;
                    converted += 1;
                }
                b'f' | b'e' | b'E' | b'g' | b'G' => {
                    skip_whitespace(input, &mut at);
                    let end = stop(input, at, width);
                    let Some((value, used)) = float(&input[at..end]) else {
                        return Ok((converted, at));
                    };
                    at += used;
                    let pointer = read(memory, next)?;
                    if length == Length::Long {
                        // `%lf` and `%Lf` assign a `double`, `%f` a `float`.
                        let bits = value.to_bits();
                        memory.write_u32(pointer, bits as u32)?;
                        memory.write_u32(pointer + 4, (bits >> 32) as u32)?;
                    } else {
                        memory.write_u32(pointer, (value as f32).to_bits())?;
                    }
                    converted += 1;
                }
                b'n' => {
                    // How much has been read so far, into an `int`, and not a
                    // conversion of its own.
                    let pointer = read(memory, next)?;
                    memory.write_u32(pointer, at as u32)?;
                }
                _ => return Ok((converted, at)),
            }
        }
        Ok((converted, at))
    }

    fn skip_whitespace(input: &[u8], at: &mut usize) {
        while input.get(*at).is_some_and(|byte| byte.is_ascii_whitespace()) {
            *at += 1;
        }
    }

    /// Where a field ends: at `width` characters, or at the first whitespace.
    fn stop(input: &[u8], at: usize, width: Option<usize>) -> usize {
        match width {
            Some(width) => at.saturating_add(width).min(input.len()),
            None => input[at..]
                .iter()
                .position(|byte| byte.is_ascii_whitespace())
                .map_or(input.len(), |offset| at + offset),
        }
    }

    /// An integer field, and how many bytes spelled it.
    ///
    /// `auto_radix` is `%i`, where a leading `0x` means hexadecimal and a leading
    /// `0` octal -- which is how a data file writes `0x1f` and `0755`.
    fn number(input: &[u8], radix: u32, auto_radix: bool) -> Option<(u64, usize)> {
        let mut used = 0usize;
        let mut negative = false;
        match input.first() {
            Some(b'-') => {
                negative = true;
                used = 1;
            }
            Some(b'+') => used = 1,
            _ => {}
        }
        let mut radix = radix;
        if input.get(used) == Some(&b'0') {
            match input.get(used + 1) {
                // `0x` is a prefix `%x` and `%i` both take, as `strtoul` does.
                Some(&next) if matches!(next, b'x' | b'X') && (auto_radix || radix == 16) => {
                    radix = 16;
                    used += 2;
                }
                _ if auto_radix => radix = 8,
                _ => {}
            }
        }
        let mut value = 0u64;
        let mut digits = 0usize;
        while let Some(&byte) = input.get(used) {
            let Some(digit) = (byte as char).to_digit(radix) else {
                break;
            };
            value = value.wrapping_mul(u64::from(radix)).wrapping_add(u64::from(digit));
            used += 1;
            digits += 1;
        }
        if digits == 0 {
            return None;
        }
        let value = if negative {
            (value as i64).wrapping_neg() as u64
        } else {
            value
        };
        Some((value, used))
    }

    /// A floating-point field: the longest prefix that is a number, so an exponent
    /// and a sign are part of it and a trailing `abc` is not.
    fn float(input: &[u8]) -> Option<(f64, usize)> {
        let mut end = input.len();
        while end > 0 {
            if let Ok(text) = std::str::from_utf8(&input[..end]) {
                if let Ok(value) = text.parse::<f64>() {
                    return Some((value, end));
                }
            }
            end -= 1;
        }
        None
    }

    /// Writes a scanned integer through the caller's pointer, in the width the
    /// length modifier asked for.
    fn store(memory: &mut GuestMemory, next: &mut u32, length: Length, value: u64) -> Result<()> {
        let pointer = read(memory, next)?;
        match length {
            Length::Short => memory.write_u16(pointer, value as u16),
            Length::Wide => {
                memory.write_u32(pointer, value as u32)?;
                memory.write_u32(pointer + 4, (value >> 32) as u32)
            }
            _ => memory.write_u32(pointer, value as u32),
        }
    }

    /// The next argument, a dword of the guest's stack.
    fn read(memory: &GuestMemory, next: &mut u32) -> Result<u32> {
        let value = memory.read_u32(*next)?;
        *next = next.wrapping_add(4);
        Ok(value)
    }

    /// A width or precision field, and how many bytes spelled it.
    fn digits(format: &[u8], at: usize) -> (Option<usize>, usize) {
        let mut value: Option<usize> = None;
        let mut moved = 0;
        while let Some(&byte) = format.get(at + moved) {
            let Some(digit) = (byte as char).to_digit(10) else {
                break;
            };
            value = Some(value.unwrap_or(0) * 10 + digit as usize);
            moved += 1;
        }
        (value, moved)
    }

    fn signed(memory: &GuestMemory, next: &mut u32, length: Length) -> Result<i64> {
        let low = read(memory, next)?;
        Ok(match length {
            Length::Short => i64::from(low as u16 as i16),
            Length::Wide => {
                let high = read(memory, next)?;
                ((u64::from(high) << 32) | u64::from(low)) as i64
            }
            _ => i64::from(low as i32),
        })
    }

    fn unsigned(memory: &GuestMemory, next: &mut u32, length: Length) -> Result<u64> {
        let low = read(memory, next)?;
        Ok(match length {
            Length::Short => u64::from(low as u16),
            Length::Wide => {
                let high = read(memory, next)?;
                (u64::from(high) << 32) | u64::from(low)
            }
            _ => u64::from(low),
        })
    }

    /// A `double` is eight bytes on the stack, low dword first.
    fn double(memory: &GuestMemory, next: &mut u32) -> Result<f64> {
        let low = u64::from(read(memory, next)?);
        let high = u64::from(read(memory, next)?);
        Ok(f64::from_bits((high << 32) | low))
    }

    /// Integer digits padded to the precision, which is a minimum and which asks
    /// for nothing at all when the value is zero and the precision is zero.
    fn with_precision(digits: String, spec: &Spec, zero: bool) -> String {
        match spec.precision {
            Some(0) if zero => String::new(),
            Some(precision) => format!(
                "{}{digits}",
                "0".repeat(precision.saturating_sub(digits.len()))
            ),
            None => digits,
        }
    }

    /// Pads `text` to the width. `lead` is how many bytes of a prefix -- a sign,
    /// `0x` -- stay in front of any zero padding.
    fn justified(text: Vec<u8>, lead: usize, spec: &Spec, zeros: bool) -> Vec<u8> {
        let width = spec.width.unwrap_or(0);
        if text.len() >= width {
            return text;
        }
        let fill = width - text.len();
        if spec.left {
            let mut out = text;
            out.extend(std::iter::repeat(b' ').take(fill));
            return out;
        }
        if zeros {
            let mut out = text[..lead].to_vec();
            out.extend(std::iter::repeat(b'0').take(fill));
            out.extend_from_slice(&text[lead..]);
            return out;
        }
        let mut out = vec![b' '; fill];
        out.extend_from_slice(&text);
        out
    }

    fn fixed(value: f64, precision: usize) -> Vec<u8> {
        if !value.is_finite() {
            return non_finite(value);
        }
        format!("{value:.precision$}").into_bytes()
    }

    /// MSVC prints its own tokens for the values IEEE calls infinite and NaN, and
    /// a game that logs one of them expects to see these.
    fn non_finite(value: f64) -> Vec<u8> {
        let text = if value.is_nan() {
            "1.#QNAN"
        } else if value < 0.0 {
            "-1.#INF"
        } else {
            "1.#INF"
        };
        text.as_bytes().to_vec()
    }

    fn scientific(value: f64, precision: usize, upper: bool) -> Vec<u8> {
        if !value.is_finite() {
            return non_finite(value);
        }
        // Rust's exponent form is `1.2345e3`; C writes `1.2345e+03`.
        let text = format!("{value:.precision$e}");
        let (mantissa, exponent) = text.split_once('e').unwrap_or((text.as_str(), "0"));
        let exponent: i32 = exponent.parse().unwrap_or(0);
        let sign = if exponent < 0 { '-' } else { '+' };
        let marker = if upper { 'E' } else { 'e' };
        format!("{mantissa}{marker}{sign}{:02}", exponent.unsigned_abs()).into_bytes()
    }

    /// `%g`: the shorter of the two forms, with the trailing zeroes of the
    /// mantissa removed.
    fn general(value: f64, precision: usize, upper: bool) -> Vec<u8> {
        if !value.is_finite() {
            return non_finite(value);
        }
        let precision = precision.max(1);
        let exponent = if value == 0.0 {
            0
        } else {
            value.abs().log10().floor() as i32
        };
        if exponent < -4 || exponent >= precision as i32 {
            strip_trailing_zeros(scientific(value, precision - 1, upper))
        } else {
            let decimals = (precision as i32 - 1 - exponent).max(0) as usize;
            strip_trailing_zeros(fixed(value, decimals))
        }
    }

    fn strip_trailing_zeros(text: Vec<u8>) -> Vec<u8> {
        let text = String::from_utf8_lossy(&text).into_owned();
        let split = text.find(['e', 'E']).unwrap_or(text.len());
        let (body, exponent) = text.split_at(split);
        let mut body = body.to_string();
        if body.contains('.') {
            while body.ends_with('0') {
                body.pop();
            }
            if body.ends_with('.') {
                body.pop();
            }
        }
        format!("{body}{exponent}").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{HEAP_BASE, STACK_BASE};

    /// Memory the shims can allocate from, and a stack area to put arguments and
    /// strings on.
    fn machine() -> (Host, GuestMemory) {
        let memory = GuestMemory::empty(HEAP_BASE as usize + 0x1000).expect("blank memory");
        (Host::new(), memory)
    }

    fn args(memory: &mut GuestMemory, values: &[u32]) -> u32 {
        let at = STACK_BASE - 0x200;
        for (index, value) in values.iter().enumerate() {
            memory.write_u32(at + index as u32 * 4, *value).unwrap();
        }
        at
    }

    #[test]
    fn a_command_line_is_split_the_way_windows_splits_one() {
        assert_eq!(
            split_command_line(r#"  "C:\Games\My Game\game.exe"  -window  "two words" "#),
            vec!["C:\\Games\\My Game\\game.exe", "-window", "two words"]
        );
        // An empty pair of quotes is an empty argument, not nothing.
        assert_eq!(split_command_line("\"\""), vec![""]);
        assert!(split_command_line("   ").is_empty());
    }

    #[test]
    fn getmainargs_answers_argc_argv_and_a_null_environment() {
        // A name with a space in it is quoted on the command line, and the quotes
        // are not part of the argument the guest sees.
        let target: &'static crate::target::Target = Box::leak(Box::new(crate::target::Target {
            name: "test",
            exe_name: "Stick Soldiers II.exe",
            ..crate::target::Target::EMPTY
        }));
        let mut host = Host::with_target(target);
        let mut memory = GuestMemory::empty(HEAP_BASE as usize + 0x1000).expect("blank memory");
        let argc_out = STACK_BASE - 0x800;
        let argv_out = STACK_BASE - 0x804;
        let env_out = STACK_BASE - 0x808;
        let at = args(&mut memory, &[argc_out, argv_out, env_out, 0, 0]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 20 };

        assert_eq!(main_args(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(argc_out).unwrap(), 1);
        let argv = call.memory.read_u32(argv_out).unwrap();
        let first = call.memory.read_u32(argv).unwrap();
        assert_eq!(call.memory.read_u32(argv + 4).unwrap(), 0, "argv is terminated");
        assert_eq!(
            cstr_bytes(call.memory, first).unwrap(),
            b"Stick Soldiers II.exe"
        );
        // The environment is a block that holds one null pointer.
        let environ = call.memory.read_u32(env_out).unwrap();
        assert_eq!(call.memory.read_u32(environ).unwrap(), 0);
    }

    #[test]
    fn an_initterm_walk_runs_one_entry_per_pass() {
        let (mut host, mut memory) = machine();
        let table = STACK_BASE - 0x600;
        memory.write_u32(table, 0x0040_1000).unwrap();
        memory.write_u32(table + 4, 0).unwrap();
        memory.write_u32(table + 8, 0x0040_2000).unwrap();
        let at = args(&mut memory, &[table, table + 12]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };

        let first = initterm(&mut host, &mut call).unwrap();
        assert_eq!(first, Return(0));
        let request = host.guest_call.take().expect("the first initialiser");
        assert_eq!(request.entry, 0x0040_1000);
        assert!(request.resume, "the walk must run again after it returns");
        assert_eq!(host.crt.atexit.len(), 0);

        // The session re-runs the shim when the routine returns.
        host.resumed = true;
        assert_eq!(initterm(&mut host, &mut call).unwrap(), Return(0));
        let request = host.guest_call.take().expect("the second initialiser");
        assert_eq!(request.entry, 0x0040_2000);

        // The last pass finishes the import and asks for nothing.
        host.resumed = true;
        assert_eq!(initterm(&mut host, &mut call).unwrap(), Return(0));
        assert!(host.guest_call.is_none(), "the walk is over");
        assert!(host.crt.walk.is_none());
    }

    #[test]
    fn initterm_e_stops_on_a_failing_initialiser() {
        let (mut host, mut memory) = machine();
        let table = STACK_BASE - 0x600;
        memory.write_u32(table, 0x0040_1000).unwrap();
        memory.write_u32(table + 4, 0x0040_2000).unwrap();
        let at = args(&mut memory, &[table, table + 8]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };

        assert_eq!(initterm_e(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(host.guest_call.take().unwrap().entry, 0x0040_1000);

        // The first initialiser failed, so the walk stops and reports it.
        host.resumed = true;
        host.guest_result = 7;
        assert_eq!(initterm_e(&mut host, &mut call).unwrap(), Return(7));
        assert!(host.guest_call.is_none(), "the second entry is not reached");
        assert!(host.crt.walk.is_none());
    }

    #[test]
    fn exit_runs_the_handlers_in_reverse() {
        let (mut host, mut memory) = machine();
        let at = args(&mut memory, &[7]);
        host.crt.atexit = vec![0x0040_1000, 0x0040_2000];
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 4 };

        assert_eq!(exit(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(host.guest_call.take().unwrap().entry, 0x0040_2000);
        host.resumed = true;
        assert_eq!(exit(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(host.guest_call.take().unwrap().entry, 0x0040_1000);
        host.resumed = true;
        // The last handler has run: the process ends with the code it asked for.
        assert_eq!(exit(&mut host, &mut call).unwrap(), Action::Exit(7));
        assert!(host.crt.atexit.is_empty());
    }

    #[test]
    fn a_cstring_is_measured_in_bytes_not_characters() {
        let (mut host, mut memory) = machine();
        // "cà" in Latin-1 is two bytes, one of which is not ASCII.
        memory.write_bytes(STACK_BASE - 0x100, &[0x63, 0xe0, 0]).unwrap();
        let at = args(&mut memory, &[STACK_BASE - 0x100]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 4 };
        assert_eq!(strlen(&mut host, &mut call).unwrap(), Return(2));
    }

    /// `_acmdln` is a variable, so its slot must hold the address of a pointer,
    /// not a stub: the guest loads the slot and dereferences it. Skipped unless a
    /// real image is supplied, because a slot is only a slot inside an image's
    /// import table.
    #[test]
    fn acmdln_is_filed_as_a_variable() {
        let Some(image) = crate::reference_image() else {
            return;
        };
        let Some(slot) = image
            .imports
            .iter()
            .find(|import| import.name == "_acmdln")
        else {
            return;
        };
        let iat_va = slot.iat_va;
        let slots: Vec<crate::dispatch::Slot> = image
            .imports
            .iter()
            .map(|import| crate::dispatch::Slot {
                dll: Box::leak(import.dll.clone().into_boxed_str()),
                name: Box::leak(import.name.clone().into_boxed_str()),
                iat_va: import.iat_va,
                args: None,
                sites: 0,
                agreement: 0,
                ambiguous: 0,
            })
            .collect();
        let table = crate::dispatch::Table {
            slots: Box::leak(slots.into_boxed_slice()),
        };
        let mut memory = GuestMemory::load(image).expect("the image maps");
        for slot in table.iter() {
            memory
                .write_import(slot.iat_va, crate::cpu::HOST_FUNCTION_BASE)
                .expect("every slot takes a stub");
        }
        let mut host = Host::new();
        fill_data_slots(&mut host, &mut memory, table).expect("the data slots fill");

        let variable = memory.read_u32(iat_va).expect("the slot reads");
        assert!(
            variable >= crate::memory::HEAP_BASE && variable < crate::memory::MEMORY_TOP,
            "the slot holds a guest address, not a stub: {variable:#010x}"
        );
        let line = memory.read_u32(variable).expect("the variable reads");
        assert_eq!(cstr_bytes(&memory, line).unwrap(), b"\"program.exe\"");
    }

    /// A formatter case: the format, the arguments it consumes, and what C would
    /// have written.
    fn rendered(format: &str, arguments: &[u32]) -> String {
        let mut memory = GuestMemory::empty(HEAP_BASE as usize + 0x1000).expect("blank memory");
        let at = STACK_BASE - 0x400;
        for (index, value) in arguments.iter().enumerate() {
            memory.write_u32(at + index as u32 * 4, *value).unwrap();
        }
        let text = format::render(&memory, format.as_bytes(), at).expect("it renders");
        String::from_utf8(text).expect("the output is text")
    }

    #[test]
    fn the_printf_conversions_the_games_use_are_rendered() {
        assert_eq!(rendered("plain text", &[]), "plain text");
        assert_eq!(rendered("%d", &[7]), "7");
        assert_eq!(rendered("%d", &[(-7_i32) as u32]), "-7");
        assert_eq!(rendered("%+d", &[7]), "+7");
        assert_eq!(rendered("%u", &[(-1_i32) as u32]), "4294967295");
        assert_eq!(rendered("%x", &[255]), "ff");
        assert_eq!(rendered("%X", &[255]), "FF");
        assert_eq!(rendered("%#x", &[255]), "0xff");
        assert_eq!(rendered("%08x", &[255]), "000000ff");
        assert_eq!(rendered("%-4d|", &[7]), "7   |");
        assert_eq!(rendered("%4d|", &[7]), "   7|");
        assert_eq!(rendered("%.3d", &[7]), "007");
        assert_eq!(rendered("%c%c", &[u32::from(b'a'), u32::from(b'b')]), "ab");
        assert_eq!(rendered("100%%", &[]), "100%");
        assert_eq!(rendered("%hd", &[0x0001_0007]), "7");
    }

    #[test]
    fn strings_and_floats_are_rendered() {
        let mut memory = GuestMemory::empty(HEAP_BASE as usize + 0x1000).expect("blank memory");
        let text_at = STACK_BASE - 0x800;
        memory.write_bytes(text_at, b"Cave Story\0").unwrap();
        let at = STACK_BASE - 0x400;
        memory.write_u32(at, text_at).unwrap();
        // 1.5 as a double: low dword, then high.
        memory.write_u32(at + 4, 0).unwrap();
        memory.write_u32(at + 8, 0x3ff8_0000).unwrap();
        memory.write_u32(at + 12, 5).unwrap();
        assert_eq!(
            String::from_utf8(format::render(&memory, b"%s", at).unwrap()).unwrap(),
            "Cave Story"
        );
        assert_eq!(
            String::from_utf8(format::render(&memory, b"%.4s", at).unwrap()).unwrap(),
            "Cave"
        );
        assert_eq!(
            String::from_utf8(format::render(&memory, b"%.2f", at + 4).unwrap()).unwrap(),
            "1.50"
        );
        // An `int` and a `double` in one call: the double's eight bytes follow the
        // dword the string pointer used.
        memory.write_u32(at + 12, 42).unwrap();
        memory.write_u32(at + 16, 0).unwrap();
        memory.write_u32(at + 20, 0x3ff8_0000).unwrap();
        assert_eq!(
            String::from_utf8(format::render(&memory, b"%d|%.1f", at + 12).unwrap()).unwrap(),
            "42|1.5"
        );
        // A conversion this host does not implement is echoed, not dropped.
        assert_eq!(
            String::from_utf8(format::render(&memory, b"%n", at).unwrap()).unwrap(),
            "%n"
        );
    }

    #[test]
    fn fopen_reads_back_what_fwrite_wrote() {
        let (mut host, mut memory) = machine();
        let path_at = STACK_BASE - 0x900;
        let mode_at = STACK_BASE - 0x8c0;
        memory.write_bytes(path_at, b"prelog.txt\0").unwrap();
        memory.write_bytes(mode_at, b"w+\0").unwrap();
        let at = args(&mut memory, &[path_at, mode_at]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };

        let file = match fopen(&mut host, &mut call).unwrap() {
            Return(file) => file,
            other => panic!("fopen returned {other:?}"),
        };
        assert_ne!(file, 0, "the file opens");
        // The block is the CRT's layout, and `feof` is a bit in it.
        assert_eq!(call.memory.read_u32(file + FILE_FLAG).unwrap(), IO_READ | IO_WRITE);

        let line = STACK_BASE - 0x700;
        call.memory.write_bytes(line, b"1\n").unwrap();
        let write_at = args(&mut call.memory, &[line, 1, 2, file]);
        let mut write = Call { memory: &mut call.memory, args: write_at, arg_bytes: 16 };
        assert_eq!(fwrite(&mut host, &mut write).unwrap(), Return(2));

        // Seeking to the start clears the cursor, and a short read sets the
        // end-of-file bit the guest's own `feof` macro looks at.
        let seek_at = args(&mut call.memory, &[file, 0, 0]);
        let mut seek = Call { memory: &mut call.memory, args: seek_at, arg_bytes: 12 };
        assert_eq!(fseek(&mut host, &mut seek).unwrap(), Return(0));

        let buffer = STACK_BASE - 0x600;
        let read_at = args(&mut call.memory, &[buffer, 1, 8, file]);
        let mut reader = Call { memory: &mut call.memory, args: read_at, arg_bytes: 16 };
        assert_eq!(fread(&mut host, &mut reader).unwrap(), Return(2));
        assert_eq!(reader.memory.read_bytes(buffer, 2).unwrap(), b"1\n");
        assert_eq!(reader.memory.read_u32(file + FILE_FLAG).unwrap() & IO_EOF, IO_EOF);

        let close_at = args(&mut reader.memory, &[file]);
        let mut close = Call { memory: &mut reader.memory, args: close_at, arg_bytes: 4 };
        assert_eq!(fclose(&mut host, &mut close).unwrap(), Return(0));
        // The bytes are the guest's now: the page persists them as its own copy.
        assert_eq!(host.files.contents("prelog.txt"), Some(b"1\n".as_slice()));
    }

    #[test]
    fn append_mode_writes_at_the_end_whatever_the_cursor_says() {
        let (mut host, mut memory) = machine();
        host.files.provide("log.txt", b"start".to_vec());
        let path_at = STACK_BASE - 0x900;
        let mode_at = STACK_BASE - 0x8c0;
        memory.write_bytes(path_at, b"log.txt\0").unwrap();
        memory.write_bytes(mode_at, b"a\0").unwrap();
        let at = args(&mut memory, &[path_at, mode_at]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        let Return(file) = fopen(&mut host, &mut call).unwrap() else {
            panic!("the append open failed");
        };
        let line = STACK_BASE - 0x700;
        call.memory.write_bytes(line, b"!").unwrap();
        let write_at = args(&mut call.memory, &[line, 1, 1, file]);
        let mut write = Call { memory: &mut call.memory, args: write_at, arg_bytes: 16 };
        assert_eq!(fwrite(&mut host, &mut write).unwrap(), Return(1));
        assert_eq!(host.files.contents("log.txt"), Some(b"start!".as_slice()));
    }

    #[test]
    fn fgets_hands_back_one_line_with_its_newline() {
        let (mut host, mut memory) = machine();
        host.files.provide("script.txt", b"one\ntwo\nlast".to_vec());
        let path_at = STACK_BASE - 0x900;
        let mode_at = STACK_BASE - 0x8c0;
        memory.write_bytes(path_at, b"script.txt\0").unwrap();
        memory.write_bytes(mode_at, b"r\0").unwrap();
        let at = args(&mut memory, &[path_at, mode_at]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        let Return(file) = fopen(&mut host, &mut call).unwrap() else {
            panic!("the file opens");
        };

        let buffer = STACK_BASE - 0x700;
        let read_at = args(&mut call.memory, &[buffer, 8, file]);
        let mut read = Call { memory: &mut call.memory, args: read_at, arg_bytes: 12 };
        assert_eq!(fgets(&mut host, &mut read).unwrap(), Return(buffer));
        assert_eq!(cstr_bytes(read.memory, buffer).unwrap(), b"one\n");
        assert_eq!(fgets(&mut host, &mut read).unwrap(), Return(buffer));
        assert_eq!(cstr_bytes(read.memory, buffer).unwrap(), b"two\n");
        // The last line has no newline: what is left is the line, and then the end.
        assert_eq!(fgets(&mut host, &mut read).unwrap(), Return(buffer));
        assert_eq!(cstr_bytes(read.memory, buffer).unwrap(), b"last");
        assert_eq!(read.memory.read_u32(file + FILE_FLAG).unwrap() & IO_EOF, IO_EOF);
        assert_eq!(fgets(&mut host, &mut read).unwrap(), Return(0));
    }

    #[test]
    fn sscanf_assigns_the_fields_a_data_line_is_made_of() {
        let (mut host, mut memory) = machine();
        let input_at = STACK_BASE - 0x900;
        let format_at = STACK_BASE - 0x8c0;
        memory.write_bytes(input_at, b"12,-34 word").unwrap();
        memory.write_bytes(input_at + 11, b"\0").unwrap();
        memory.write_bytes(format_at, b"%d,%d %s\0").unwrap();
        let first = STACK_BASE - 0x700;
        let second = STACK_BASE - 0x704;
        let text = STACK_BASE - 0x780;
        let at = args(&mut memory, &[input_at, format_at, first, second, text]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        assert_eq!(sscanf(&mut host, &mut call).unwrap(), Return(3));
        assert_eq!(call.memory.read_u32(first).unwrap(), 12);
        assert_eq!(call.memory.read_u32(second).unwrap() as i32, -34);
        assert_eq!(cstr_bytes(call.memory, text).unwrap(), b"word");
    }

    #[test]
    fn sscanf_reads_hex_floats_and_widths() {
        let (mut host, mut memory) = machine();
        let input_at = STACK_BASE - 0x900;
        let format_at = STACK_BASE - 0x8c0;
        // `0x1f` is hexadecimal to `%x`, the float keeps its sign and exponent, and
        // a width bounds the field.
        memory.write_bytes(input_at, b"0x1f -1.5e2 12ab\0").unwrap();
        memory.write_bytes(format_at, b"%x %f %5s\0").unwrap();
        let hex = STACK_BASE - 0x700;
        let real = STACK_BASE - 0x704;
        let text = STACK_BASE - 0x780;
        let at = args(&mut memory, &[input_at, format_at, hex, real, text]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        assert_eq!(sscanf(&mut host, &mut call).unwrap(), Return(3));
        assert_eq!(call.memory.read_u32(hex).unwrap(), 0x1f);
        // A `%f` assigns a `float`, which is one dword.
        let value = f32::from_bits(call.memory.read_u32(real).unwrap());
        assert!((value + 150.0).abs() < 1e-4, "{value}");
        assert_eq!(cstr_bytes(call.memory, text).unwrap(), b"12ab");

        // `%lf` is the one that assigns a `double`, and a field that is not a
        // number stops the count where it stands.
        call.memory.write_bytes(input_at, b"2.5 nope").unwrap();
        call.memory.write_bytes(input_at + 9, b"\0").unwrap();
        call.memory.write_bytes(format_at, b"%lf %d\0").unwrap();
        let wide = STACK_BASE - 0x740;
        let at = args(&mut call.memory, &[input_at, format_at, wide, hex]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 8 };
        assert_eq!(sscanf(&mut host, &mut call).unwrap(), Return(1));
        let bits = (u64::from(call.memory.read_u32(wide + 4).unwrap()) << 32)
            | u64::from(call.memory.read_u32(wide).unwrap());
        assert_eq!(f64::from_bits(bits), 2.5);
    }

    #[test]
    fn fscanf_leaves_the_stream_where_the_last_field_ended() {
        let (mut host, mut memory) = machine();
        host.files.provide("stage.txt", b"7 8 tail".to_vec());
        let path_at = STACK_BASE - 0x900;
        let mode_at = STACK_BASE - 0x8c0;
        memory.write_bytes(path_at, b"stage.txt\0").unwrap();
        memory.write_bytes(mode_at, b"r\0").unwrap();
        let at = args(&mut memory, &[path_at, mode_at]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        let Return(file) = fopen(&mut host, &mut call).unwrap() else {
            panic!("the file opens");
        };

        let format_at = STACK_BASE - 0x880;
        call.memory.write_bytes(format_at, b"%d %d\0").unwrap();
        let first = STACK_BASE - 0x700;
        let second = STACK_BASE - 0x704;
        let at = args(&mut call.memory, &[file, format_at, first, second]);
        let mut read = Call { memory: &mut call.memory, args: at, arg_bytes: 8 };
        assert_eq!(fscanf(&mut host, &mut read).unwrap(), Return(2));
        assert_eq!(read.memory.read_u32(first).unwrap(), 7);
        assert_eq!(read.memory.read_u32(second).unwrap(), 8);

        // The space after the second number is still in the file, so the line read
        // next starts with it -- which is what the CRT does.
        let buffer = STACK_BASE - 0x780;
        let read_at = args(&mut read.memory, &[buffer, 16, file]);
        let mut line = Call { memory: &mut read.memory, args: read_at, arg_bytes: 12 };
        assert_eq!(fgets(&mut host, &mut line).unwrap(), Return(buffer));
        assert_eq!(cstr_bytes(line.memory, buffer).unwrap(), b" tail");

        // Reading on past the end is the failure the caller's loop checks.
        let at = args(&mut line.memory, &[file, format_at, first, second]);
        let mut again = Call { memory: &mut line.memory, args: at, arg_bytes: 8 };
        assert_eq!(fscanf(&mut host, &mut again).unwrap(), Return(EOF));
        assert_eq!(again.memory.read_u32(file + FILE_FLAG).unwrap() & IO_EOF, IO_EOF);

        // A character that does not match the format is a count of zero, not the
        // end of the file: the caller's loop can tell the two apart.
        host.files.provide("letters.txt", b"abc".to_vec());
        let path_at = STACK_BASE - 0x900;
        call.memory.write_bytes(path_at, b"letters.txt\0").unwrap();
        let at = args(&mut call.memory, &[path_at, mode_at]);
        let mut open = Call { memory: &mut call.memory, args: at, arg_bytes: 8 };
        let Return(letters) = fopen(&mut host, &mut open).unwrap() else {
            panic!("the second file opens");
        };
        let at = args(&mut open.memory, &[letters, format_at, first, second]);
        let mut refused = Call { memory: &mut open.memory, args: at, arg_bytes: 8 };
        assert_eq!(fscanf(&mut host, &mut refused).unwrap(), Return(0));
        assert_eq!(
            refused.memory.read_u32(letters + FILE_FLAG).unwrap() & IO_EOF,
            0,
            "the input did not run out, it stopped matching"
        );
    }

    #[test]
    fn the_clock_is_the_instant_getlocaltime_reports() {
        let (mut host, mut memory) = machine();
        let when = STACK_BASE - 0x700;
        let at = args(&mut memory, &[when]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 4 };
        assert_eq!(
            time64(&mut host, &mut call).unwrap(),
            Action::Return64(946_728_000),
            "2000-01-01 12:00:00, in edx:eax"
        );
        assert_eq!(call.memory.read_u32(when).unwrap(), 946_728_000);
        assert_eq!(call.memory.read_u32(when + 4).unwrap(), 0);

        let at = args(&mut call.memory, &[0]);
        let mut none = Call { memory: &mut call.memory, args: at, arg_bytes: 4 };
        assert_eq!(time64(&mut host, &mut none).unwrap(), Action::Return64(946_728_000));

        // `struct tm`: seconds, minutes, the hour of noon, the first of January,
        // January, 2000, a Saturday, its day of the year, no daylight saving.
        let at = args(&mut none.memory, &[when]);
        let mut call = Call { memory: &mut none.memory, args: at, arg_bytes: 4 };
        let Return(tm) = localtime64(&mut host, &mut call).unwrap() else {
            panic!("localtime answered")
        };
        let field = |index: u32| call.memory.read_u32(tm + index * 4).unwrap();
        assert_eq!(field(0), 0);
        assert_eq!(field(2), 12, "the hour");
        assert_eq!(field(3), 1, "the day");
        assert_eq!(field(4), 0, "January is zero");
        assert_eq!(field(5), 100, "the year since 1900");
        assert_eq!(field(6), 6, "a Saturday");
        // The block is the CRT's single static one, not one per call.
        let at = args(&mut call.memory, &[when]);
        let mut again = Call { memory: &mut call.memory, args: at, arg_bytes: 4 };
        assert_eq!(localtime64(&mut host, &mut again).unwrap(), Return(tm));
    }

    #[test]
    fn the_working_directory_is_the_bundle() {
        let (mut host, mut memory) = machine();
        let buffer = STACK_BASE - 0x700;
        let at = args(&mut memory, &[buffer, 16]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        assert_eq!(getcwd(&mut host, &mut call).unwrap(), Return(buffer));
        assert_eq!(cstr_bytes(call.memory, buffer).unwrap(), b".");
        // A buffer that cannot hold `"."` and its terminator is the failure the CRT
        // reports, rather than a write past the end of it.
        let at = args(&mut call.memory, &[buffer, 1]);
        let mut small = Call { memory: &mut call.memory, args: at, arg_bytes: 8 };
        assert_eq!(getcwd(&mut host, &mut small).unwrap(), Return(0));
        // There is one directory, so changing to another succeeds and changes
        // nothing.
        let at = args(&mut small.memory, &[STACK_BASE - 0x900]);
        let mut change = Call { memory: &mut small.memory, args: at, arg_bytes: 4 };
        assert_eq!(chdir(&mut host, &mut change).unwrap(), Return(0));
    }

    #[test]
    fn the_vc8_start_up_entry_points_answer() {
        let (mut host, mut memory) = machine();
        // `_controlfp_s` reports the control word through its pointer, and 0 for
        // success.
        let word = STACK_BASE - 0x700;
        let at = args(&mut memory, &[word, 0x27f, 0x3_0000]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 12 };
        assert_eq!(controlfp_s(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(word).unwrap(), DEFAULT_CONTROL_WORD);

        // The C locale's characters are bytes, and nothing debugs this machine.
        let at = args(&mut call.memory, &[0x82u32]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 4 };
        assert_eq!(is_mbblead(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(crt_debugger_hook(&mut host, &mut call).unwrap(), Return(0));

        // The two dispatchers only ever say "not handled here": a fault is this
        // machine's own stop, not something the guest's frames unwind.
        let at = args(&mut call.memory, &[0xc000_0005, STACK_BASE - 0x780]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 8 };
        assert_eq!(xcpt_filter(&mut host, &mut call).unwrap(), Return(0));
        let at = args(&mut call.memory, &[0; 4]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 16 };
        assert_eq!(cxx_frame_handler(&mut host, &mut call).unwrap(), Return(1));
        let at = args(&mut call.memory, &[0; 6]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 24 };
        assert_eq!(except_handler(&mut host, &mut call).unwrap(), Return(1));

        // The two that never return: the runtime's last report and the
        // invalid-parameter handler.
        let at = args(&mut call.memory, &[8]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 4 };
        assert_eq!(amsg_exit(&mut host, &mut call).unwrap(), Action::Exit(255));
        assert!(host.notices.iter().any(|notice| notice.contains("R6008")));
        let at = args(&mut call.memory, &[0; 5]);
        let mut call = Call { memory: &mut call.memory, args: at, arg_bytes: 20 };
        assert_eq!(invoke_watson(&mut host, &mut call).unwrap(), Action::Exit(EXIT_FAILURE));
    }

    #[test]
    fn a_double_result_goes_to_the_float_stack() {
        let (mut host, mut memory) = machine();
        // 3.0 as a double: low dword, then high.
        let at = args(&mut memory, &[0, 0x4008_0000]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 8 };
        match sqrt(&mut host, &mut call).unwrap() {
            Action::Float(crate::cpu::FloatOp::Push(bits)) => {
                assert!((f64::from_bits(bits) - 3.0_f64.sqrt()).abs() < 1e-12);
            }
            other => panic!("sqrt answered {other:?}"),
        }
        // The FPU instrinsics take their arguments from the stack the shim cannot
        // see, so they name the operation instead of a value.
        assert_eq!(
            ci_pow(&mut host, &mut call).unwrap(),
            Action::Float(crate::cpu::FloatOp::Pow)
        );
        assert_eq!(
            ci_acos(&mut host, &mut call).unwrap(),
            Action::Float(crate::cpu::FloatOp::Unary(crate::cpu::Unary::Acos))
        );
        assert_eq!(
            ftol(&mut host, &mut call).unwrap(),
            Action::Float(crate::cpu::FloatOp::TruncateToInt)
        );
    }

    #[test]
    fn malloc_and_free_share_the_heap_arena() {
        let (mut host, mut memory) = machine();
        let at = args(&mut memory, &[64]);
        let mut call = Call { memory: &mut memory, args: at, arg_bytes: 4 };
        let block = match malloc(&mut host, &mut call).unwrap() {
            Return(block) => block,
            other => panic!("malloc returned {other:?}"),
        };
        assert_eq!(host.allocated(block), Some(64));
        call.write_u32(at, block).unwrap();
        assert_eq!(free(&mut host, &mut call).unwrap(), Return(0));
        assert_eq!(host.allocated(block), None);
    }
}
