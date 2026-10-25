//! The facts about the program being run.
//!
//! Everything in this crate is a Win32 machine: the interpreter, the PE loader,
//! guest memory, the shim framework, and the DLL behaviour that is true of any
//! 32-bit Windows program. What is *not* true of any program is which binary is
//! running, how big its screen is, which shims it needs, and which files stand in
//! for devices it cannot have. Those are supplied here, once, by the port that
//! owns the target.
//!
//! Keeping them in one value is what lets this crate be reused: a port carries a
//! [`Target`] and nothing else about itself, and the engine never learns a
//! product name.

use crate::dispatch::Table;
use crate::shim::Binding;

/// One program the machine runs.
pub struct Target {
    /// Label for diagnostics: the port, not the executable (`example`).
    pub name: &'static str,
    /// The file name the guest sees for itself: `GetModuleFileNameA`,
    /// `GetCommandLineA` and the window-class checks read this. It is the
    /// *loaded* name, so a split build reports `Game-vi.exe` and not the
    /// name of some other build in the same folder.
    pub exe_name: &'static str,
    /// The screen the guest asks for, used as the default before its own
    /// `SetDisplayMode` and for rectangle answers that have no window yet.
    pub screen: (u16, u16, u8),
    /// This executable's import surface. Empty for a port that has not generated
    /// one yet, which makes every import an unanswered [`crate::session::Outcome::HostCall`].
    pub table: Table,
    /// Shim tables, in resolution order: later tables override earlier ones, so a
    /// port appends its own table after the generic ones it reuses.
    pub shims: &'static [&'static [Binding]],
    /// Files the host supplies with a length and no bytes, standing in for a
    /// device the browser cannot have (a disc marker, a launcher). The guest
    /// measures these; it does not read them.
    pub provided_files: &'static [(&'static str, u32)],
}

impl Target {
    /// A machine with no image loaded: every lookup empty, every subsystem at
    /// its default. What the unit tests in this crate run against.
    pub const EMPTY: Target = Target {
        name: "unset",
        exe_name: "program.exe",
        screen: (640, 480, 8),
        table: Table::EMPTY,
        shims: crate::shim::TABLES,
        provided_files: &[],
    };
}

/// The target a session uses when its caller did not name one.
pub static DEFAULT: Target = Target::EMPTY;
