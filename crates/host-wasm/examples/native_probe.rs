//! Boots a game natively with the same setup the wasm ABI uses, and prints what
//! the display holds. Used to tell an engine problem from a module problem.
//!
//!   cargo run --example native_probe -- <install-dir> [instructions] [exe-name]
//!
//! An install with more than one program at its root (`unins000.exe` beside the
//! game, or a launcher) needs the third argument, or the first one the folder
//! lists is booted.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use host::dispatch::{Slot, Table};
use host::pe;
use host::session::Session;
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

/// Walks the install tree, so a game that reads `data/Head.tsc` finds it: the
/// file table is keyed by path, and a flat listing only has the root.
fn collect(
    directory: &Path,
    prefix: &str,
    wanted: Option<&str>,
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
            collect(&path, &relative, wanted, files, executable);
            continue;
        }
        let is_program = prefix.is_empty() && name.to_ascii_lowercase().ends_with(".exe");
        let chosen = match wanted {
            Some(wanted) => name.eq_ignore_ascii_case(wanted),
            // The first root program the folder lists, which is why a named one
            // is usually needed.
            None => executable.is_none(),
        };
        if is_program && chosen {
            *executable = Some((name, fs::read(&path).expect("the executable reads")));
            continue;
        }
        files.push((relative, fs::read(&path).expect("a file reads")));
    }
}

/// Whether the guest reached the budget without presenting a frame, which is the
/// shape of a stall rather than a slow machine.
fn ddraw_is_stalled(session: &Session) -> bool {
    session.host.ddraw.flips() == 0
}

/// Writes the presented pixels as a 24-bit `.bmp`, which is the one format this
/// crate can hand a viewer without a second decoder.
fn write_frame(session: &Session, path: &Path) {
    let Some((buffer, width, height)) = session.host.ddraw.framebuffer() else {
        println!("  no frame to write: the guest has no display");
        return;
    };
    let pitch = session.host.ddraw.framebuffer_pitch();
    let bits = session.host.ddraw.bits();
    let offset = buffer.wrapping_sub(pe::IMAGE_BASE) as usize;
    let memory = session.memory.as_slice();
    let bytes = u32::from(bits / 8)
        .max(1);
    let stride = width as usize * 3;
    let stride = (stride + 3) / 4 * 4;
    let mut image = vec![0_u8; stride * height as usize];
    let palette = session.host.ddraw.palette().to_vec();
    for row in 0..height as usize {
        // A DIB stores its bottom row first.
        let source = offset + (height as usize - 1 - row) * pitch as usize;
        let target = row * stride;
        for column in 0..width as usize {
            let (red, green, blue) = match bits {
                8 => {
                    let entry = palette[memory[source + column] as usize];
                    (entry[0], entry[1], entry[2])
                }
                16 => {
                    let at = source + column * 2;
                    let value = u16::from_le_bytes([memory[at], memory[at + 1]]);
                    (
                        (((value >> 11) & 0x1f) as u8) << 3,
                        (((value >> 5) & 0x3f) as u8) << 2,
                        ((value & 0x1f) as u8) << 3,
                    )
                }
                _ => {
                    let at = source + column * bytes as usize;
                    (memory[at + 2], memory[at + 1], memory[at])
                }
            };
            image[target + column * 3] = blue;
            image[target + column * 3 + 1] = green;
            image[target + column * 3 + 2] = red;
        }
    }
    let header = 14 + 40;
    let mut out = Vec::with_capacity(header + image.len());
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((header + image.len()) as u32).to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&(header as u32).to_le_bytes());
    out.extend_from_slice(&40_u32.to_le_bytes());
    out.extend_from_slice(&(width as i32).to_le_bytes());
    out.extend_from_slice(&(height as i32).to_le_bytes());
    out.extend_from_slice(&1_u16.to_le_bytes());
    out.extend_from_slice(&24_u16.to_le_bytes());
    out.extend_from_slice(&0_u32.to_le_bytes());
    out.extend_from_slice(&(image.len() as u32).to_le_bytes());
    for _ in 0..4 {
        out.extend_from_slice(&0_u32.to_le_bytes());
    }
    out.extend_from_slice(&image);
    match fs::write(path, out) {
        Ok(()) => println!("  wrote {width}x{height} to {}", path.display()),
        Err(error) => println!("  could not write {}: {error}", path.display()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(directory) = args.first() else {
        eprintln!("usage: native_probe <install-dir> [instructions]");
        return ExitCode::from(2);
    };
    let budget: u64 = args
        .get(1)
        .and_then(|value| value.parse().ok())
        .unwrap_or(120_000_000);
    let directory = PathBuf::from(directory);
    let wanted = args.get(2).map(String::as_str);
    if let Some(wanted) = wanted {
        if !directory.join(wanted).is_file() {
            eprintln!("{wanted} is not in {}", directory.display());
            return ExitCode::from(2);
        }
    }

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut executable = None;
    collect(&directory, "", wanted, &mut files, &mut executable);
    let Some((name, bytes)) = executable else {
        eprintln!("no .exe at the root of {}", directory.display());
        return ExitCode::from(2);
    };
    println!("booting {name} ({} bytes)", bytes.len());

    let image = pe::Image::parse(bytes.clone()).expect("the image parses");
    // `PROBE_SCREEN=<w>x<h>x<d>` seeds the mode the guest sees before it asks
    // for one: a game that fails to create an 8-bit surface only tells its story
    // at its own depth.
    let screen = std::env::var("PROBE_SCREEN")
        .ok()
        .and_then(|text| {
            let parts: Vec<u32> = text
                .split(['x', 'X', '*'])
                .filter_map(|part| part.trim().parse().ok())
                .collect();
            match parts.as_slice() {
                [width, height, depth] => Some((*width, *height, *depth)),
                _ => None,
            }
        })
        .map(|(width, height, depth)| (width as u16, height as u16, depth as u8))
        .unwrap_or((640, 480, 8));
    let target: &'static Target = Box::leak(Box::new(Target {
        name: "probe",
        exe_name: leak(&name),
        screen,
        table: table_for(&image),
        shims: host::shim::TABLES,
        provided_files: &[],
    }));

    let mut session = Session::load_for(bytes, target).expect("the session loads");
    // `PROBE_SKIP=<substring,...>` withholds files, which tells a game that
    // waits on one apart from a game that is stuck for its own reasons.
    let skip: Vec<String> = std::env::var("PROBE_SKIP")
        .map(|value| value.split(',').map(|part| part.trim().to_lowercase()).filter(|part| !part.is_empty()).collect())
        .unwrap_or_default();
    for (file, contents) in files {
        let lowered = file.to_lowercase();
        if skip.iter().any(|part| lowered.contains(part)) {
            println!("  withheld {file}");
            continue;
        }
        session.host.files.provide(&file, contents);
    }

    // A game that asks for its own clock needs one to move, or its animation and
    // its start-up delays never finish. Thirty frames per million instructions is
    // roughly what the browser's per-frame budget buys, and it keeps the probe's
    // timing in the same range as the page's.
    let mut ran = 0;
    let mut samples: std::collections::BTreeMap<(u32, String), u32> = Default::default();
    let mut frames: std::collections::HashSet<u64> = Default::default();
    let mut walk_printed = 0;
    // Notices are printed at the end by default; a run long enough to be
    // measured in minutes needs them as they happen.
    let live_notices = std::env::var("PROBE_NOTICES").is_ok();
    let mut printed_notices = 0;
    // `PROBE_IMPORTS` prints each import the first time it is seen, so a run that
    // stops drawing shows the order it got there in instead of only its last call.
    let log_imports = std::env::var("PROBE_IMPORTS").is_ok();
    let mut seen_imports: std::collections::HashSet<String> = Default::default();
    // Sampling builds a string per chunk, which is a lot of work over billions
    // of instructions: only a run that reads the samples pays for it.
    let sampling = std::env::var("PROBE_SAMPLES").is_ok();
    // `PROBE_FRAMES=<dir>` writes the screen every hundred chunks, so a long
    // load can be watched instead of guessed at.
    let frame_dir = std::env::var("PROBE_FRAMES").ok().map(PathBuf::from);
    let mut chunk = 200_000_u64;
    if std::env::var("PROBE_WALK").is_ok() {
        chunk = 3_000;
    }
    // `PROBE_CHUNK` sets the slice between samples: a small one makes
    // `PROBE_IMPORTS` see the calls a chunk boundary would otherwise hide.
    if let Ok(value) = std::env::var("PROBE_CHUNK") {
        if let Ok(value) = value.parse::<u64>() {
            chunk = value.max(1);
        }
    }
    // `PROBE_PRESS=<ms>` presses and releases space, return and the left mouse
    // button once, at that point on the guest's clock: a screen that waits for
    // input cannot be told from a screen that is stuck without one.
    let press_at = std::env::var("PROBE_PRESS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok());
    let mut pressed = 0_u8;
    // `PROBE_CLOCK` divides the instruction count into milliseconds: the default
    // 33 000 is the exchange rate the page uses, and a larger divisor runs the
    // guest's clock faster, which tells a slow guest from a stopped one.
    let clock = std::env::var("PROBE_CLOCK")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(33_000)
        .max(1);
    while ran < budget {
        session.set_time_ms(ran / clock);
        if let Some(at) = press_at {
            let now = ran / clock;
            if pressed == 0 && now >= at {
                pressed = 1;
                session.host.key(0x39, true);
                session.host.key(0x1c, true);
                // `Z`, which is what a Cave Story menu confirms with: a run that
                // only presses space and return sits on the title for ever.
                session.host.key(0x2c, true);
                // In the middle of the window, where a menu's first item sits: a
                // button at the top-left corner is a click on nothing.
                session.host.pointer(396, 274);
                session.host.pointer_button(0, true);
                println!("  pressed space, return and the left button at {ran}");
            } else if pressed == 1 && now >= at + 50 {
                pressed = 2;
                session.host.key(0x39, false);
                session.host.key(0x1c, false);
                session.host.key(0x2c, false);
                session.host.pointer_button(0, false);
            }
        }
        let outcome = session.run(chunk as u32);
        if sampling {
            *samples
                .entry((
                    session.pc(),
                    session
                        .call_stack(3)
                        .iter()
                        .map(|frame| format!("{frame:#x}"))
                        .collect::<Vec<_>>()
                        .join("<-"),
                ))
                .or_default() += 1;
        }
        if log_imports {
            if let Some(label) = &session.last_import {
                let name = label.split(" args=").next().unwrap_or(label).to_string();
                if seen_imports.insert(name.clone()) && seen_imports.len() <= 400 {
                    println!("{ran:12} import {name}");
                }
            }
        }
        if live_notices && session.host.notices.len() > printed_notices {
            for notice in &session.host.notices[printed_notices..] {
                println!("{ran:12} notice {notice}");
            }
            printed_notices = session.host.notices.len();
        }
        if let Some(directory) = &frame_dir {
            if ran / chunk.max(1) % 100 == 0 {
                write_frame(&session, &directory.join(format!("{ran:012}.bmp")));
            }
        }
        // A picture that keeps changing is a guest that is drawing, whether or not
        // it ever calls `Flip`: a direct blit to the primary surface presents
        // without touching the flip counter.
        if ran / chunk as u64 % 100 == 0 {
            if let Some(buffer) = session.host.ddraw.primary_buffer() {
                let offset = buffer.wrapping_sub(pe::IMAGE_BASE) as usize;
                let memory = session.memory.as_slice();
                let mut hash = 0xcbf2_9ce4_8422_2325_u64;
                for index in (0..640 * 480).step_by(7) {
                    hash ^= u64::from(memory[offset + index]);
                    hash = hash.wrapping_mul(0x1000_0000_01b3);
                }
                frames.insert(hash);
            }
        }
        // `PROBE_TRACE` prints what the guest was doing at even intervals, which
        // is how a run that neither stops nor presents a frame is read.
        if std::env::var("PROBE_TRACE").is_ok() && ran % (chunk * 100) == 0 {
            println!(
                "{ran:12} pc {:#010x} {}",
                session.pc(),
                session.last_import.clone().unwrap_or_default()
            );
        }
        if std::env::var("PROBE_WALK").is_ok() && samples.len() + walk_printed < 200 {
            walk_printed += 1;
            println!(
                "{ran:9} pc {:#010x} {}",
                session.pc(),
                session.last_import.clone().unwrap_or_default()
            );
        }
        if outcome.code() != 0 {
            // Why it stopped is the whole point of the probe: a fault names the
            // instruction, a host call names the import the shims did not answer.
            println!("stopped at {ran}: {}", outcome.describe(target.table));
            if let Some(last) = &session.last_import {
                println!("  last import: {last}");
            }
            println!(
                "  pc {:#010x}, {} instructions",
                session.pc(),
                session.instructions()
            );
            break;
        }
        ran += chunk;
    }
    for notice in &session.host.notices {
        println!("notice {notice}");
    }

    println!("  {} distinct frames", frames.len());

    // What the page would have shown, written out as a bitmap so a run can be
    // looked at rather than only measured.
    if let Ok(path) = std::env::var("PROBE_FRAME") {
        write_frame(&session, Path::new(&path));
    }

    // `PROBE_MEMORY=<path>` writes the guest's memory as it stands, so a surface
    // the guest filled can be looked at where it lives: `0xADDRESS` in a log is
    // an offset into this file at `ADDRESS - 0x400000`.
    if let Ok(path) = std::env::var("PROBE_MEMORY") {
        let memory = session.memory.as_slice();
        if let Err(error) = fs::write(&path, memory) {
            println!("  could not write {path}: {error}");
        } else {
            println!(
                "  memory at {path}: {} bytes, indexed from {:#010x}",
                memory.len(),
                pe::IMAGE_BASE
            );
        }
    }

    // `PROBE_PALETTE` prints the palette the guest set, which is the only place
    // an eight-bit-minded guest's "colour 0" is written down.
    if std::env::var("PROBE_PALETTE").is_ok() {
        let palette = session.host.ddraw.palette();
        for (index, entry) in palette.iter().enumerate().take(16) {
            println!(
                "  palette {index:3} = ({}, {}, {})",
                entry[0], entry[1], entry[2]
            );
        }
    }

    if let Ok(addresses) = std::env::var("PROBE_PEEK") {
        for address in addresses.split(',') {
            // `+n` follows one pointer: `0x458b00+0` is `*(u32*)0x458b00`.
            let (base, hops) = match address.split_once('+') {
                Some((base, hops)) => (base, hops.parse::<u32>().unwrap_or(0)),
                None => (address, 0),
            };
            let base = base.trim_start_matches("0x");
            let mut value = session
                .memory
                .read_u32(u32::from_str_radix(base, 16).expect("PROBE_PEEK is a hex address"))
                .unwrap_or(0);
            for _ in 0..hops {
                value = session.memory.read_u32(value).unwrap_or(0);
            }
            println!("  peek {address} = {value:#010x}");
        }
    }

    // What the guest wrote, which is the only way to see a log stream work: the
    // bytes stay in the host's file table, not in the install folder.
    if std::env::var("PROBE_SAVED").is_ok() {
        for (path, bytes) in session.host.files.saved_files() {
            let head = String::from_utf8_lossy(&bytes[..bytes.len().min(120)]);
            println!("  saved {path} ({} bytes) {head:?}", bytes.len());
        }
    }

    // `PROBE_SURFACES` prints every surface the guest created, with the address
    // of its pixels: a surface that never reaches the screen can be looked at
    // where it lives instead, with `PROBE_MEMORY`.
    if std::env::var("PROBE_SURFACES").is_ok() {
        let mut surfaces = session.host.ddraw.surface_buffers();
        surfaces.sort_by_key(|(buffer, _, _, _)| *buffer);
        for (buffer, width, height, locks) in surfaces {
            println!("  surface {buffer:#010x} {width}x{height} locks {locks}");
        }
        let ddraw = &session.host.ddraw;
        for (label, handle) in [("primary", ddraw.primary_surface()), ("back", ddraw.back_surface())] {
            match handle.and_then(|handle| ddraw.surface(handle).map(|shape| (handle, shape))) {
                Some((handle, (buffer, width, height, _pitch, bits))) => println!(
                    "  {label} {handle:#010x} buffer {buffer:#010x} {width}x{height}x{bits}"
                ),
                None => println!("  {label} none"),
            }
        }
    }

    if std::env::var("PROBE_SAMPLES").is_ok() {
        let mut top: Vec<_> = samples.iter().collect();
        top.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        let total: u32 = samples.values().sum();
        println!("  {total} samples, {} distinct", samples.len());
        for ((pc, stack), count) in top.iter().take(20) {
            println!("  {count:5} x pc {pc:#010x} in {stack}");
        }
    }

    if ddraw_is_stalled(&session) {
        println!(
            "  stalled at {:#010x}, called from {:x?} after {}",
            session.pc(),
            session.call_stack(8),
            session.last_import.clone().unwrap_or_else(|| "no import".to_string())
        );
    }

    let ddraw = &session.host.ddraw;
    let lit = |buffer: Option<u32>| {
        buffer.map_or(0, |buffer| {
            let offset = buffer.wrapping_sub(pe::IMAGE_BASE) as usize;
            let memory = session.memory.as_slice();
            (0..640 * 480)
                .filter(|index| memory[offset + index] != 0)
                .count()
        })
    };
    let locks: u32 = ddraw.surface_buffers().iter().map(|(_, _, _, locks)| locks).sum();
    println!(
        "ran {ran}, flips {}, locks {locks}, surfaces {}, primary {:?} ({} lit), back {:?} ({} lit)",
        ddraw.flips(),
        ddraw.surface_count(),
        ddraw.primary_buffer(),
        lit(ddraw.primary_buffer()),
        ddraw.back_buffer(),
        lit(ddraw.back_buffer()),
    );
    let _ = Path::new("");
    ExitCode::SUCCESS
}
