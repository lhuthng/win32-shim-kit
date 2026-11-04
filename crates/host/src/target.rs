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

/// A guest routine the host runs natively when the guest reaches its exact
/// entry address. Which kinds exist is [`crate::hle`]'s business; the target
/// only says which addresses its own image carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hle {
    /// The reference image's LZW decoder.
    Lzw,
    /// The reference image's sprite blitter.
    Blit { keyed: bool },
    /// Madagascar's FPU vertex transform.
    MatTransform,
    /// Hercules' keyed sprite row, `0x0040cbf4`: skips source byte 0.
    HerculesKeyedRow,
    /// Hercules' palette-remapped sprite row, `0x0040ca51`.
    HerculesRemapRow,
    /// Hercules' scaled sprite row, `0x0040e56c`.
    HerculesScaledRow,
}

/// One native replacement: the guest address it answers for, and what runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HleEntry {
    pub address: u32,
    pub kind: Hle,
}

/// The Windows the guest is told it runs on.
///
/// A start-up check reads this before it does anything else, and an era-era
/// title reads it in ways that are exact rather than sensible: a 2002 launcher
/// that refuses to boot on a machine reporting Windows XP is answered with the
/// Windows 98 it was written for, and gets as far as its own window.
///
/// The packing is the part that has to be right, because a guest reads
/// `GetVersion` as one number: bit 31 of the low word is set on NT and clear on
/// Win9x (and on Win32s, which is why `5.1` is `0x8501` and not `0x0501`), the
/// major version is the low word's second byte, the minor version its first, and
/// the build is the high word. With the bytes the other way round the low word
/// reads `0x0105` -- version 1.5 with the Win32s bit clear -- and an old
/// platform check concludes it is running on Windows 3.1 and gives up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    pub major: u8,
    pub minor: u8,
    pub build: u16,
    /// `VER_PLATFORM_WIN32_WINDOWS` (1) or `VER_PLATFORM_WIN32_NT` (2), the value
    /// `GetVersionEx` reports and the one bit 31 of `GetVersion` follows.
    pub platform: u32,
}

impl Windows {
    pub const WIN98: Windows = Windows {
        major: 4,
        minor: 10,
        build: 2222,
        platform: 1,
    };
    pub const NT4: Windows = Windows {
        major: 4,
        minor: 0,
        build: 1381,
        platform: 2,
    };
    pub const WIN2000: Windows = Windows {
        major: 5,
        minor: 0,
        build: 2195,
        platform: 2,
    };
    /// Windows XP, 5.1 build 2600: what a guest is told when its target does not
    /// say otherwise.
    pub const XP: Windows = Windows {
        major: 5,
        minor: 1,
        build: 2600,
        platform: 2,
    };

    /// `GetVersion`'s packed DWORD for this platform.
    pub const fn code(&self) -> u32 {
        let nt = if self.platform == 2 { 0x8000 } else { 0 };
        ((self.build as u32) << 16) | nt | ((self.major as u32) << 8) | self.minor as u32
    }
}

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
    /// The Windows `GetVersion` and `GetVersionEx` answer with: a title whose
    /// start-up reads the platform only runs on the one it names here.
    pub windows: Windows,
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
    /// Guest routines the host runs natively for this image, by entry address.
    /// An address is only ever matched for the target that declared it, so two
    /// images that happen to put code at the same address never cross.
    pub hle: &'static [HleEntry],
}

impl Target {
    /// A machine with no image loaded: every lookup empty, every subsystem at
    /// its default. What the unit tests in this crate run against.
    pub const EMPTY: Target = Target {
        name: "unset",
        exe_name: "program.exe",
        screen: (640, 480, 8),
        windows: Windows::XP,
        table: Table::EMPTY,
        shims: crate::shim::TABLES,
        provided_files: &[],
        hle: &[],
    };
}

/// The target a session uses when its caller did not name one: the reference
/// image's own import surface is empty, and its native replacements are armed,
/// because a session built this way is a session against that image.
pub static DEFAULT: Target = Target {
    name: "unset",
    exe_name: "program.exe",
    screen: (640, 480, 8),
    windows: Windows::XP,
    table: Table::EMPTY,
    shims: crate::shim::TABLES,
    provided_files: &[],
    hle: crate::hle::REFERENCE_HLE,
};
