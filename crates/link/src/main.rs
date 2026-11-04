//! Converts a game's own DLLs into modules this host can map.
//!
//! ```text
//! shimkit-link <install-dir> <out-dir>
//! ```
//!
//! A Windows game that keeps its engine in `Eng1.dll` is not one executable: the
//! launcher calls `LoadLibraryA` and then resolves an entry point out of the DLL.
//! The host has one image, mapped at [`host::pe::IMAGE_BASE`], and every DLL that
//! pulled in `msvcrt` was linked to prefer `0x10000000` -- which lands inside this
//! host's heap arena. So a DLL has to be *placed* before it can run, and that is
//! what this tool does:
//!
//! - it packs the modules into the window between the program's image and the
//!   TEB, where the host can map them and the heap never reaches;
//! - it applies each DLL's base relocations once, at build time, and rewrites the
//!   header's `ImageBase`, so the interpreter never runs a relocation pass;
//! - it reports which of each module's imports the shims answer, because a
//!   module with an unanswered import is refused at load rather than left to
//!   call a null.
//!
//! The output folder is a complete install: every file is hard-linked (or copied)
//! from the source, and only the modules are new bytes. Bake that folder instead
//! of the original and the payload carries the placed modules.
//!
//! `modules.json` is the build's record: what was placed where, what it exports,
//! and what it still needs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use host::memory::MEMORY_TOP;
use host::pe::{Image, IMAGE_BASE};

/// The lowest address a module is placed at.
///
/// Modules are packed *down* from [`WINDOW_END`], so this is a sanity floor
/// rather than a position: it keeps a pathological install from covering the
/// stack, the TEB and the heap base, all of which live below `0x0100_0000`.
const WINDOW_START: u32 = 0x2000_0000;

/// The highest a module may end at: the end of guest memory.
///
/// The free space just above the program is not usable -- this game's image
/// declares 8 MB and the TEB follows it at `0x00ff_f000` -- so modules go where
/// the heap can only arrive after allocating everything the arena holds.
const WINDOW_END: u32 = MEMORY_TOP;

/// Modules are placed on the addressing granularity Windows uses.
const GRANULARITY: u32 = 0x1_0000;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(install), Some(out)) = (args.first(), args.get(1)) else {
        eprintln!("usage: shimkit-link <install-dir> <out-dir>");
        return ExitCode::from(2);
    };
    let install = PathBuf::from(install);
    let out = PathBuf::from(out);
    if !install.is_dir() {
        eprintln!("no such folder: {}", install.display());
        return ExitCode::from(2);
    }

    // The program decides where the window starts: a module may not be placed
    // inside the image it is loaded beside.
    let Some(exe) = pick_executable(&install) else {
        eprintln!("no .exe at the root of {}", install.display());
        return ExitCode::from(2);
    };
    let program = match Image::from_path(&exe) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{}: {error}", exe.display());
            return ExitCode::from(1);
        }
    };
    let program_end = IMAGE_BASE.wrapping_add(program.size_of_image);
    let mut cursor = align_down(WINDOW_END, GRANULARITY);
    println!(
        "program {} ends at {program_end:#010x}; modules pack down from {cursor:#010x}",
        exe.file_name().unwrap_or_default().to_string_lossy()
    );
    // The program's own surface is the other half of "will this run": a module
    // whose imports are answered still cannot boot if the launcher itself stops
    // on the first import nobody answers.
    let (answered, missing) = coverage(&program);
    println!(
        "program imports: {answered}/{} answered",
        program.imports.len()
    );
    for (dll, functions) in &missing {
        println!(
            "  {dll} unanswered ({}): {}",
            functions.len(),
            functions.join(", ")
        );
    }

    if let Err(error) = mirror(&install, &out) {
        eprintln!("{error}");
        return ExitCode::from(1);
    }

    let mut placed = Vec::new();
    let mut failure = false;
    for name in modules_at(&install) {
        match place(&name, &install.join(&name), cursor) {
            Ok(module) => {
                if let Err(error) = fs::write(out.join(&name), &module.bytes) {
                    eprintln!("{name}: {error}");
                    failure = true;
                    continue;
                }
                println!(
                    "  {:<12} {:#010x} {:>8} bytes  {} export(s), {}/{} imports answered",
                    name,
                    module.base,
                    module.size,
                    module.exports.len(),
                    module.answered,
                    module.imports
                );
                for (dll, functions) in &module.missing {
                    println!("      {dll} unanswered: {}", functions.join(", "));
                }
                cursor = module.base;
                placed.push(module.facts);
            }
            Err(error) => {
                eprintln!("{name}: {error}");
                failure = true;
            }
        }
    }

    let manifest = serde_json::json!({
        "kind": "modules",
        "program": {
            "name": exe.file_name().unwrap_or_default().to_string_lossy(),
            "ends": program_end,
            "image_bytes": program.size_of_image,
        },
        "window": { "start": WINDOW_START, "end": WINDOW_END },
        "modules": placed,
    });
    let text = serde_json::to_string_pretty(&manifest).unwrap_or_default();
    if let Err(error) = fs::write(out.join("modules.json"), format!("{text}\n")) {
        eprintln!("{}: {error}", out.join("modules.json").display());
        return ExitCode::from(1);
    }
    println!(
        "placed {} module(s) into {}; modules.json written",
        placed.len(),
        out.display()
    );
    if failure {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

/// One module the compiler produced.
struct Placed {
    bytes: Vec<u8>,
    base: u32,
    size: u32,
    exports: Vec<(String, u32)>,
    imports: usize,
    answered: usize,
    missing: BTreeMap<String, Vec<String>>,
    facts: serde_json::Value,
}

/// Places one DLL below `top`: relocates it there and checks what it will ask
/// for.
fn place(name: &str, path: &Path, top: u32) -> Result<Placed, String> {
    let bytes = fs::read(path).map_err(|error| error.to_string())?;
    let image = Image::parse(bytes.clone())?;
    let size = image.size_of_image;
    let span = align_up(size, GRANULARITY);
    let base = top
        .checked_sub(span)
        .ok_or_else(|| "there is no room left below the window top".to_string())?;
    if size == 0 {
        return Err("the image declares no size".to_string());
    }
    let end = base
        .checked_add(size)
        .ok_or_else(|| format!("placing it at {base:#010x} wraps the address space"))?;
    if base < WINDOW_START || end > WINDOW_END {
        return Err(format!(
            "it does not fit: {base:#010x}..{end:#010x} is outside the module window \
             {WINDOW_START:#010x}..{WINDOW_END:#010x} ({} bytes)",
            WINDOW_END - WINDOW_START
        ));
    }

    let mut bytes = bytes;
    let delta = base.wrapping_sub(image.image_base);
    if delta != 0 {
        if image.relocations.is_empty() {
            return Err(format!(
                "it prefers {:#010x} and has no base relocations, so it cannot be moved",
                image.image_base
            ));
        }
        relocate(&mut bytes, &image, delta)?;
        // A module is only loadable if the bytes and the header agree: the host
        // maps it where the header says.
        let check = Image::parse(bytes.clone())?;
        if check.image_base != base {
            return Err(format!(
                "after relocation the header says {:#010x}, not {base:#010x}",
                check.image_base
            ));
        }
        if check.entry_point != image.entry_point.wrapping_add(delta) {
            return Err("the entry point did not move with the image".to_string());
        }
    }

    let (answered, missing) = coverage(&image);

    let exports: Vec<(String, u32)> = image
        .exports
        .iter()
        .map(|(export, rva)| (export.clone(), base.wrapping_add(*rva)))
        .collect();
    let facts = serde_json::json!({
        "name": name,
        "base": base,
        "bytes": size,
        "preferred": image.image_base,
        "moved": delta != 0,
        "relocations": image.relocations.len(),
        "entry": base.wrapping_add(image.entry_point.wrapping_sub(image.image_base)),
        "exports": exports
            .iter()
            .map(|(export, address)| serde_json::json!({ "name": export, "address": address }))
            .collect::<Vec<_>>(),
        "imports": {
            "total": image.imports.len(),
            "answered": answered,
            "missing": missing,
        },
    });
    Ok(Placed {
        bytes,
        base,
        size,
        exports,
        imports: image.imports.len(),
        answered,
        missing,
        facts,
    })
}

/// Which of an image's imports the shim tables answer, and which they do not.
fn coverage(image: &Image) -> (usize, BTreeMap<String, Vec<String>>) {
    let mut answered = 0;
    let mut missing: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for import in &image.imports {
        let slot = host::dispatch::Slot {
            dll: leak(&import.dll),
            name: leak(&import.name),
            iat_va: import.iat_va,
            args: None,
            sites: 0,
            agreement: 0,
            ambiguous: 0,
        };
        if host::shim::binding(host::shim::TABLES, &slot).is_some() {
            answered += 1;
        } else {
            missing
                .entry(import.dll.clone())
                .or_default()
                .push(import.name.clone());
        }
    }
    for functions in missing.values_mut() {
        functions.sort();
        functions.dedup();
    }
    (answered, missing)
}

/// Applies every absolute address the image records, and the header's base.
///
/// This is the whole reason the tool exists: the interpreter maps an image where
/// its header says, with no relocation pass, so a module that could not be placed
/// at build time would have to fault at load time.
fn relocate(bytes: &mut [u8], image: &Image, delta: u32) -> Result<(), String> {
    for rva in &image.relocations {
        let offset = image
            .rva_to_offset(*rva)
            .ok_or_else(|| format!("relocation at RVA {rva:#x} has no file offset"))?;
        let end = offset + 4;
        if end > bytes.len() {
            return Err(format!("relocation at RVA {rva:#x} runs past the file"));
        }
        let value = u32::from_le_bytes(bytes[offset..end].try_into().unwrap());
        bytes[offset..end].copy_from_slice(&value.wrapping_add(delta).to_le_bytes());
    }
    // `e_lfanew` names the PE header; the optional header's `ImageBase` is 28
    // bytes past the COFF header that follows it.
    let pe = u32::from_le_bytes(
        bytes
            .get(0x3c..0x40)
            .ok_or("the DOS header is short")?
            .try_into()
            .unwrap(),
    ) as usize;
    let at = pe + 24 + 28;
    let field = bytes
        .get_mut(at..at + 4)
        .ok_or("the optional header is short")?;
    field.copy_from_slice(&image.image_base.wrapping_add(delta).to_le_bytes());
    Ok(())
}

/// The root `.exe` the payload boots: `SHIMKIT_EXE` when set, otherwise the
/// first one in name order, which is the same rule bake uses.
fn pick_executable(install: &Path) -> Option<PathBuf> {
    let mut names: Vec<String> = Vec::new();
    for entry in fs::read_dir(install).ok()? {
        let entry = entry.ok()?;
        if !entry.file_type().ok()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(".exe") {
            names.push(name);
        }
    }
    names.sort();
    let wanted = std::env::var("SHIMKIT_EXE").ok();
    let chosen = match wanted {
        Some(name) if names.iter().any(|known| known.eq_ignore_ascii_case(&name)) => name,
        _ => names.into_iter().next()?,
    };
    Some(install.join(chosen))
}

/// The root `.dll` files, in name order.
fn modules_at(install: &Path) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let Ok(listing) = fs::read_dir(install) else {
        return names;
    };
    for entry in listing.flatten() {
        if !entry.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(".dll") {
            names.push(name);
        }
    }
    names.sort();
    names
}

/// Reproduces the install at `out`, hard-linking where the filesystem allows it
/// and copying where it does not.
///
/// A hard link costs nothing and keeps the 200 MB of the game's data out of the
/// compiler's way; the modules are the only files it rewrites, and it writes
/// those after the mirror.
fn mirror(install: &Path, out: &Path) -> Result<(), String> {
    fs::create_dir_all(out).map_err(|error| format!("{}: {error}", out.display()))?;
    let listing = fs::read_dir(install).map_err(|error| format!("{}: {error}", install.display()))?;
    for entry in listing {
        let entry = entry.map_err(|error| error.to_string())?;
        let name = entry.file_name();
        let source = entry.path();
        let target = out.join(&name);
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            let target = if target.exists() {
                target
            } else {
                fs::create_dir_all(&target)
                    .map_err(|error| format!("{}: {error}", target.display()))?;
                target
            };
            mirror(&source, &target)?;
            continue;
        }
        if !kind.is_file() {
            continue;
        }
        if target.exists() {
            let _ = fs::remove_file(&target);
        }
        if fs::hard_link(&source, &target).is_err() {
            fs::copy(&source, &target).map_err(|error| format!("{}: {error}", target.display()))?;
        }
    }
    Ok(())
}

fn align_up(value: u32, alignment: u32) -> u32 {
    value.wrapping_add(alignment - 1) & !(alignment - 1)
}

fn align_down(value: u32, alignment: u32) -> u32 {
    value & !(alignment - 1)
}

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}
