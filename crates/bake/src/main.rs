//! Puts a game into the artifact and describes it.
//!
//! ```text
//! shimkit-bake <install-dir> <out-dir>
//! ```
//!
//! Writes:
//!   `<out>/game/`         every file the game needs, and nothing a patcher left
//!   `<out>/game/files.json`  `[{ name, bytes }]`, the list the page fetches
//!   `<out>/artifact.json`    what the build is: executable, entry point, sizes
//!
//! `artifact.json` is the answer to "what did this build produce": the bytes the
//! page will fetch, every root executable with its entry address and import
//! count, the screen the module should seed, the kit version that produced it,
//! and the module's own size. Nothing in it is game-specific except the facts
//! read out of the executables.
//!
//! A payload may hold more than one executable -- a split install names one per
//! language, and each reads its own suffixed resources -- so format 2 lists them
//! all and names a `default`. A loader boots `?exe=`, then `default`, then the
//! first name in the list; it does not sort, because the list is already sorted
//! and the default may deliberately be a later entry.
//!
//! Three environment variables override what only the build knows: `SHIMKIT_EXE`
//! names the default executable, `SHIMKIT_SCREEN` sets `<width>x<height>x<depth>`
//! (default 640x480x8, the mode the shims answer before the guest asks), and
//! `SHIMKIT_WINDOWS` says which Windows the guest is told it runs on (`98`, `me`,
//! `nt4`, `2000` or `xp`, the default). A title whose start-up refuses anything
//! newer than the Windows it was written for is the case `SHIMKIT_WINDOWS` exists
//! for.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::json;

/// Folders a patcher or an installer leaves behind, not part of the game.
const SKIP_DIRECTORIES: [&str; 3] = ["backup", "Backup", "__MACOSX"];
const SKIP_FILES: [&str; 2] = [".DS_Store", "files.json"];

/// The ABI the released module speaks. Additive exports do not change it.
const ABI: u32 = 1;
/// Artifact schema this build writes.
const ARTIFACT_FORMAT: u32 = 2;
/// The screen the module seeds before the guest asks for its own.
const DEFAULT_SCREEN: (u32, u32, u32) = (640, 480, 8);
/// The platform the module claims before the guest asks: Windows XP, 5.1 build
/// 2600, `VER_PLATFORM_WIN32_NT`.
const DEFAULT_WINDOWS: (u32, u32, u32, u32) = (5, 1, 2600, 2);

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(install), Some(out)) = (args.first(), args.get(1)) else {
        eprintln!("usage: shimkit-bake <install-dir> <out-dir>");
        return ExitCode::from(2);
    };
    let install = PathBuf::from(install);
    let out = PathBuf::from(out);
    if !install.is_dir() {
        eprintln!("no such folder: {}", install.display());
        return ExitCode::from(2);
    }
    // The output holds the install's files, so baking a payload into itself makes
    // every file its own source: the copy step deletes the target before reading
    // it and the payload eats the game it was given. A payload folder and the
    // install it was baked from are never the same directory; say so instead.
    if install
        .canonicalize()
        .ok()
        .zip(out.join("game").canonicalize().ok())
        .is_some_and(|(install, game)| install == game)
    {
        eprintln!(
            "{} is already the output of a bake; give the folder it was baked from",
            install.display()
        );
        return ExitCode::from(2);
    }

    let mut entries: Vec<(String, u64)> = Vec::new();
    if let Err(error) = walk(&install, &install, &mut entries) {
        eprintln!("error: {error}");
        return ExitCode::from(1);
    }
    if entries.is_empty() {
        eprintln!("{} has no files", install.display());
        return ExitCode::from(1);
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));

    let game = out.join("game");
    if let Err(error) = fs::create_dir_all(&game) {
        eprintln!("error: {}: {error}", game.display());
        return ExitCode::from(1);
    }
    for (name, _) in &entries {
        let target = game.join(name);
        if let Some(directory) = target.parent() {
            if let Err(error) = fs::create_dir_all(directory) {
                eprintln!("error: {}: {error}", directory.display());
                return ExitCode::from(1);
            }
        }
        // `fs::copy` keeps the source's mode, and game installs are often
        // read-only, so the copy has to be made writable: otherwise the second
        // bake into the same folder fails on its own first output.
        if target.exists() {
            let _ = fs::remove_file(&target);
        }
        if let Err(error) = fs::copy(install.join(name), &target) {
            eprintln!("error: {}: {error}", target.display());
            return ExitCode::from(1);
        }
        if let Ok(metadata) = fs::metadata(&target) {
            let mut permissions = metadata.permissions();
            if permissions.readonly() {
                permissions.set_readonly(false);
                let _ = fs::set_permissions(&target, permissions);
            }
        }
    }

    let list: Vec<serde_json::Value> = entries
        .iter()
        .map(|(name, bytes)| json!({ "name": name, "bytes": bytes }))
        .collect();
    let text = serde_json::to_string_pretty(&list).unwrap_or_else(|_| "[]".to_string());
    if let Err(error) = fs::write(game.join("files.json"), format!("{text}\n")) {
        eprintln!("error: {}: {error}", game.join("files.json").display());
        return ExitCode::from(1);
    }

    let total: u64 = entries.iter().map(|(_, bytes)| bytes).sum();
    for (name, _) in &entries {
        // A name with a space, `#`, `?` or `%` cannot be asked for by URL, so a
        // hosted payload could never fetch it. Say so here rather than at boot.
        if name.contains(|c: char| c == ' ' || c == '#' || c == '?' || c == '%') {
            eprintln!("warning: {name} cannot be fetched by URL; rename it before hosting");
        }
    }

    let mut executables: Vec<String> = entries
        .iter()
        .map(|(name, _)| name.clone())
        .filter(|name| name.to_ascii_lowercase().ends_with(".exe") && !name.contains('/'))
        .collect();
    executables.sort();

    let wanted = std::env::var("SHIMKIT_EXE").ok().filter(|name| executables.contains(name));
    if std::env::var("SHIMKIT_EXE").is_ok() && wanted.is_none() {
        eprintln!("warning: SHIMKIT_EXE is not a root executable of {}; using the first", install.display());
    }
    let default = wanted.clone().or_else(|| executables.first().cloned());

    let mut facts = Vec::new();
    for name in &executables {
        match describe_executable(&game.join(name)) {
            Ok(described) => facts.push(described),
            Err(error) => eprintln!("warning: {name}: {error}"),
        }
    }
    if facts.is_empty() {
        eprintln!("warning: no .exe at the root of {}; a loader will not boot it", install.display());
    }

    let screen = screen_setting();
    let windows = windows_setting();
    let mut artifact = json!({
        "format": ARTIFACT_FORMAT,
        "kit": { "version": env!("CARGO_PKG_VERSION"), "abi": ABI },
        "screen": { "width": screen.0, "height": screen.1, "depth": screen.2 },
        "windows": {
            "major": windows.0,
            "minor": windows.1,
            "build": windows.2,
            "platform": windows.3,
        },
        "executables": facts,
        "game": { "root": "game", "files": entries.len(), "bytes": total },
    });
    if let Some(name) = &default {
        artifact["default"] = json!(name);
    }
    let module = out.join("host_wasm.wasm");
    if let Ok(bytes) = fs::read(&module) {
        // The build stamp is the module's content, not its length: a rebuilt
        // module that happens to come out the same size is still a different
        // module, and the page keys its fetch on this so a reload cannot replay
        // the old shims.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in &bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        artifact["module"] = json!({
            "name": "host_wasm.wasm",
            "bytes": bytes.len(),
            "build": format!("{hash:016x}"),
        });
    }

    let text = serde_json::to_string_pretty(&artifact).unwrap_or_default();
    if let Err(error) = fs::write(out.join("artifact.json"), format!("{text}\n")) {
        eprintln!("error: {}: {error}", out.join("artifact.json").display());
        return ExitCode::from(1);
    }

    println!(
        "baked {} files ({:.1} MiB) into {}",
        entries.len(),
        total as f64 / 1024.0 / 1024.0,
        game.display()
    );
    ExitCode::SUCCESS
}

fn walk(root: &Path, directory: &Path, entries: &mut Vec<(String, u64)>) -> Result<(), String> {
    let listing = fs::read_dir(directory).map_err(|error| format!("{}: {error}", directory.display()))?;
    for entry in listing {
        let entry = entry.map_err(|error| error.to_string())?;
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry.file_type().map_err(|error| error.to_string())?;
        if file_type.is_dir() {
            if SKIP_DIRECTORIES.contains(&name.as_str()) {
                continue;
            }
            walk(root, &path, entries)?;
            continue;
        }
        if SKIP_FILES.contains(&name.as_str()) || !file_type.is_file() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|error| error.to_string())?
            .components()
            .map(|component| component.as_os_str().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join("/");
        let bytes = entry.metadata().map_err(|error| error.to_string())?.len();
        entries.push((relative, bytes));
    }
    Ok(())
}

/// What the host will see in this image: its entry address and import surface.
fn describe_executable(path: &Path) -> Result<serde_json::Value, String> {
    let image = host::pe::Image::from_path(path)?;
    Ok(json!({
        "name": path.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default(),
        "bytes": image.bytes.len(),
        "entry": image.entry_point,
        "entry_hex": format!("{:#010x}", image.entry_point),
        "image_bytes": image.size_of_image,
        "imports": image.imports.len(),
    }))
}

/// `SHIMKIT_WINDOWS=98|me|nt4|2000|xp`, or the platform the shims answer by
/// default. Named rather than numeric because the number a title wants is a
/// release, and because `4.10` is not the only odd thing about the era.
fn windows_setting() -> (u32, u32, u32, u32) {
    let Ok(text) = std::env::var("SHIMKIT_WINDOWS") else {
        return DEFAULT_WINDOWS;
    };
    match text.trim().to_ascii_lowercase().as_str() {
        "98" | "win98" | "windows98" => (4, 10, 2222, 1),
        "me" | "winme" => (4, 90, 3000, 1),
        "nt4" => (4, 0, 1381, 2),
        "2000" | "win2000" => (5, 0, 2195, 2),
        "xp" | "winxp" => DEFAULT_WINDOWS,
        other => {
            eprintln!(
                "warning: SHIMKIT_WINDOWS={other} is not 98, me, nt4, 2000 or xp; using XP"
            );
            DEFAULT_WINDOWS
        }
    }
}

/// `SHIMKIT_SCREEN=<w>x<h>x<d>`, or the mode the shims answer by default.
fn screen_setting() -> (u32, u32, u32) {
    let Ok(text) = std::env::var("SHIMKIT_SCREEN") else {
        return DEFAULT_SCREEN;
    };
    let parts: Vec<u32> = text
        .split(['x', 'X', '*'])
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    match parts.as_slice() {
        [width, height, depth] if *width > 0 && *height > 0 && *depth > 0 => (*width, *height, *depth),
        _ => {
            eprintln!("warning: SHIMKIT_SCREEN={text} is not <width>x<height>x<depth>; using 640x480x8");
            DEFAULT_SCREEN
        }
    }
}
