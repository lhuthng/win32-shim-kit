//! The Win32 rehost engine: guest image, memory, interpreter, and the boundary.
//!
//! This crate is the machine, not a program. It maps a PE32 image at its fixed
//! base, gives it guest linear memory with section permissions, interprets its
//! instructions, and hands every import call to a shim. Which executable is
//! running, how large its screen is, which shims it resolves, and which files
//! stand in for absent devices all come from a [`target::Target`] the port
//! supplies: nothing here names a product.
//!
//! The engine assumes a target's image has no relocation directory and is not
//! `/DYNAMICBASE`, so it is mapped once at [`pe::IMAGE_BASE`] and never moves.

pub mod cpu;
pub mod dispatch;
pub mod hle;
pub mod input;
pub mod memory;
pub mod pe;
pub mod session;
pub mod shim;
pub mod target;
pub mod thunks;

pub type Result<T> = std::result::Result<T, String>;

/// The executable to use in a test that needs a real image.
///
/// The guest is user-supplied, so this crate never names one: a test suite runs
/// without it and skips those cases, and `SHIMKIT_GUEST=/path/to/game.exe` turns
/// them on. The port that owns a table checks its table against the same binary
/// in its own tests.
#[cfg(test)]
pub(crate) fn reference_image() -> Option<pe::Image> {
    let path = std::path::PathBuf::from(std::env::var_os("SHIMKIT_GUEST")?);
    if !path.is_file() {
        eprintln!("skipping: {} is not a file", path.display());
        return None;
    }
    Some(pe::Image::from_path(&path).expect("the supplied executable parses"))
}
