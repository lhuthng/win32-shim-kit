//! Boots a game to its title, then runs the credits the way the ending script
//! does, and writes the frames out.
//!
//! Cave Story's ending does two things to start its credits: the TSC command
//! `<CRE` sets bit 3 of the flag word at `0x49e1e8`, and it calls the credits'
//! own init at `0x40d440`. This probe does those two things from the host, so the
//! credits roll through the game's own code and the engine's own shims instead of
//! through a reimplementation.
//!
//!   cargo run --example credits_probe -- <install-dir> [boot-instructions]
//!
//! Frames land in `tmp/credits/` as `<index>.raw`: three little-endian words of
//! width, height and pitch, the palette as 768 bytes, then the pixels, plus one
//! `surface-NN.raw` per surface the guest created.
//!
//! The scene does not render from here yet. Entered from the title, the credits'
//! own code runs and then asks to blit a cast picture with a source rectangle
//! whose right edge is left of its left edge -- the clip the ending leaves behind
//! is not there -- and the host stops on it. Reaching the roll needs the state a
//! finished game leaves, so this probe is the entry point, not the check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use host::dispatch::{Slot, Table};
use host::pe;
use host::session::Session;
use host::shim::GuestCall;
use host::target::Target;

fn leak(text: &str) -> &'static str {
    Box::leak(text.to_owned().into_boxed_str())
}

fn table_for(image: &pe::Image) -> Table {
    let mut imports = image.imports.clone();
    imports.sort_by_key(|import| import.iat_va);
    let slots: Vec<Slot> = imports
        .iter()
        .map(|import| Slot {
            dll: leak(&import.dll),
            name: leak(&import.name),
            iat_va: import.iat_va,
            args: None,
            sites: 0,
            agreement: 0,
            ambiguous: 0,
        })
        .collect();
    Table {
        slots: Box::leak(slots.into_boxed_slice()),
    }
}

fn collect(
    directory: &Path,
    prefix: &str,
    files: &mut Vec<(String, Vec<u8>)>,
    executable: &mut Option<(String, Vec<u8>)>,
) {
    for entry in fs::read_dir(directory).expect("the folder reads") {
        let entry = entry.expect("an entry");
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let relative = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
            collect(&path, &relative, files, executable);
            continue;
        }
        if prefix.is_empty() && name.to_ascii_lowercase().ends_with(".exe") && executable.is_none() {
            *executable = Some((name, fs::read(&path).expect("the executable reads")));
            continue;
        }
        files.push((relative, fs::read(&path).expect("a file reads")));
    }
}

/// The flag word the TSC `<CRE` command sets bit 3 of, and the credits' init.
const FLAGS: u32 = 0x0049_e1e8;
const CREDITS_BIT: u32 = 8;
const CREDITS_INIT: u32 = 0x0040_d440;

/// Writes the frame the page would show: its pixels and the palette they index.
fn save(session: &Session, index: usize) {
    let Some(buffer) = session.host.ddraw.primary_buffer() else {
        return;
    };
    let offset = buffer.wrapping_sub(pe::IMAGE_BASE) as usize;
    let memory = session.memory.as_slice();
    let width = 640_usize;
    let height = 480_usize;
    let mut out = Vec::with_capacity(12 + 768 + width * height);
    out.extend_from_slice(&(width as u32).to_le_bytes());
    out.extend_from_slice(&(height as u32).to_le_bytes());
    out.extend_from_slice(&(width as u32).to_le_bytes());
    for entry in session.host.ddraw.palette().iter() {
        out.extend_from_slice(entry);
    }
    out.extend_from_slice(&memory[offset..offset + width * height]);
    let _ = fs::create_dir_all("tmp/credits");
    let _ = fs::write(format!("tmp/credits/{index:03}.raw"), out);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(directory) = args.first() else {
        eprintln!("usage: credits_probe <install-dir> [boot-instructions]");
        return ExitCode::from(2);
    };
    let boot: u64 = args
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(430_000_000);
    let directory = PathBuf::from(directory);

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut executable = None;
    collect(&directory, "", &mut files, &mut executable);
    let Some((name, bytes)) = executable else {
        eprintln!("no .exe at the root of {}", directory.display());
        return ExitCode::from(2);
    };

    let image = pe::Image::parse(bytes.clone()).expect("the image parses");
    let target: &'static Target = Box::leak(Box::new(Target {
        name: "credits",
        exe_name: leak(&name),
        screen: (640, 480, 8),
        windows: host::target::Windows::XP,
        table: table_for(&image),
        shims: host::shim::TABLES,
        provided_files: &[],
        hle: &[],
    }));

    let mut session = Session::load_for(bytes, target).expect("the session loads");
    for (file, contents) in files {
        session.host.files.provide(&file, contents);
    }

    // The boot budget: the game renders every music track before its first screen,
    // and the title is up once it starts presenting frames.
    let mut ran = 0_u64;
    while ran < boot {
        session.set_time_ms(ran / 33_000);
        let outcome = session.run(2_000_000);
        ran += 2_000_000;
        if outcome.code() == 3 {
            println!("stopped at {ran}: {}", outcome.describe(target.table));
            if let Some(last) = &session.last_import {
                println!("  last import: {last}");
            }
            println!("  pc {:#010x}", session.pc());
            return ExitCode::from(1);
        }
    }
    println!(
        "{ran} instructions to the title, {} flip(s)",
        session.host.ddraw.flips()
    );

    // Start a new game first. The credits clip their sprites against a rectangle
    // the ending has set up; entering them from the title leaves it unset, and
    // the first blit clips against nothing.
    let window = 0x0001_0100;
    for press in 0..3 {
        session.host.input.key(0x2c, true, window);
        session.set_time_ms(ran / 33_000 + press * 100);
        session.run(3_000_000);
        session.host.input.key(0x2c, false, window);
        session.set_time_ms(ran / 33_000 + press * 100 + 50);
        session.run(9_000_000);
        ran += 12_000_000;
    }
    println!("after the title: pc {:#010x}, {} surface(s)", session.pc(), session.host.ddraw.surface_count());

    // What `<CRE` does: raise the flag the game loop tests, then enter the
    // credits' init the way the script command would.
    let flags = session.memory.read_u32(FLAGS).expect("the flag word reads");
    session
        .memory
        .write_u32(FLAGS, flags | CREDITS_BIT)
        .expect("the flag word writes");
    let esp = session.cpu.regs.esp;
    session
        .start_guest_call(
            GuestCall {
                entry: CREDITS_INIT,
                args: [0; 4],
                answer: None,
                resume: false,
            },
            esp,
            0,
        )
        .expect("the credits are entered");
    println!("entered the credits at {CREDITS_INIT:#010x} with flags {flags:#x}");

    // The credits scroll at their own pace; a frame every couple of million
    // instructions is enough to see the text move.
    let mut index = 0_usize;
    for round in 0..60 {
        session.set_time_ms(ran / 33_000 + round * 60);
        let outcome = session.run(3_000_000);
        ran += 3_000_000;
        if outcome.code() == 3 {
            println!("stopped in the credits at {ran}: {}", outcome.describe(target.table));
            if let Some(last) = &session.last_import {
                println!("  last import: {last}");
            }
            println!("  pc {:#010x}", session.pc());
            println!(
                "  called from {:x?}",
                session
                    .call_stack(10)
                    .iter()
                    .map(|frame| format!("{frame:#010x}"))
                    .collect::<Vec<_>>()
            );
            break;
        }
        save(&session, index);
        index += 1;
    }
    // Every surface, not just the presented one: the credits compose their screens
    // off-screen, and what the roll draws is what those surfaces hold.
    for (index, (buffer, width, height, _locks)) in
        session.host.ddraw.surface_buffers().iter().enumerate()
    {
        let offset = buffer.wrapping_sub(pe::IMAGE_BASE) as usize;
        let memory = session.memory.as_slice();
        if offset + (width * height) as usize > memory.len() {
            continue;
        }
        let mut out = Vec::with_capacity(12 + 768 + (width * height) as usize);
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        for entry in session.host.ddraw.palette().iter() {
            out.extend_from_slice(entry);
        }
        out.extend_from_slice(&memory[offset..offset + (width * height) as usize]);
        let _ = fs::write(format!("tmp/credits/surface-{index:02}.raw"), out);
    }

    for notice in &session.host.notices {
        println!("notice {notice}");
    }
    println!(
        "{index} frame(s) saved, {} instructions total, {} flip(s)",
        session.instructions(),
        session.host.ddraw.flips()
    );
    ExitCode::SUCCESS
}
