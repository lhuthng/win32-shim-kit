//! Boots a game natively with the same setup the wasm ABI uses, and prints what
//! the display holds. Used to tell an engine problem from a module problem.
//!
//!   cargo run --example native_probe -- <install-dir> [instructions]

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

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    let mut executable = None;
    for entry in fs::read_dir(&directory).expect("the folder reads") {
        let entry = entry.expect("an entry");
        let path = entry.path();
        if !entry.file_type().map(|kind| kind.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.to_ascii_lowercase().ends_with(".exe") && executable.is_none() {
            executable = Some((name.clone(), fs::read(path.clone()).expect("the executable reads")));
            continue;
        }
        files.push((name, fs::read(path).expect("a file reads")));
    }
    let Some((name, bytes)) = executable else {
        eprintln!("no .exe at the root of {}", directory.display());
        return ExitCode::from(2);
    };

    let image = pe::Image::parse(bytes.clone()).expect("the image parses");
    let target: &'static Target = Box::leak(Box::new(Target {
        name: "probe",
        exe_name: leak(&name),
        screen: (640, 480, 8),
        table: table_for(&image),
        shims: host::shim::TABLES,
        provided_files: &[],
    }));

    let mut session = Session::load_for(bytes, target).expect("the session loads");
    for (file, contents) in files {
        session.host.files.provide(&file, contents);
    }

    let mut ran = 0;
    while ran < budget {
        if session.run(200_000).code() != 0 {
            break;
        }
        ran += 200_000;
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
    println!(
        "ran {ran}, flips {}, surfaces {}, primary {:?} ({} lit), back {:?} ({} lit)",
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
