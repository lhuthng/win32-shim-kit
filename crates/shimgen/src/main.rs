//! Turns function maps into code or documentation, and validates them.
//!
//! ```text
//! shimgen validate examples/*.json
//! shimgen generate examples/kernel32.json --out target/kernel32.rs
//! shimgen generate examples/gtavc-inventory.json --format markdown
//! ```
//!
//! A shim map generates a Rust module: a `SHIMS` binding table plus one function
//! per import. An inventory map generates a Markdown table, because there is no
//! host to generate code for. Every map is validated first; a map with errors
//! produces no output and exits 1.
//!
//! Validation rules are the ones the kit exists to enforce: a stub needs a
//! description, an inventory entry needs a status, unknown fields are errors
//! rather than silently ignored, and a missing `evidence` is a warning.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{Map, Value};

const STUB_KINDS: [&str; 4] = ["return", "noop", "custom", "unimplemented"];
const STATUSES: [&str; 5] = ["implemented", "stub", "unimplemented", "replaced", "not-needed"];
const KINDS: [&str; 2] = ["shim", "inventory"];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first() else {
        usage();
        return ExitCode::from(2);
    };
    match command.as_str() {
        "validate" => run_validate(&args[1..]),
        "generate" => run_generate(&args[1..]),
        "help" | "--help" | "-h" => {
            usage();
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unknown command: {other}");
            usage();
            ExitCode::from(2)
        }
    }
}

fn usage() {
    println!(
        "shimgen — function maps to code or documentation\n\n\
         shimgen validate <map.json>...\n\
         shimgen generate <map.json>... [--out <file>] [--format rust|markdown]"
    );
}

fn run_validate(paths: &[String]) -> ExitCode {
    if paths.is_empty() {
        usage();
        return ExitCode::from(2);
    }
    let mut failed = false;
    let mut warnings = 0;
    for path in paths {
        let report = check_file(Path::new(path));
        for warning in &report.warnings {
            warnings += 1;
            println!("warning: {warning}");
        }
        for error in &report.errors {
            println!("error: {error}");
        }
        if report.errors.is_empty() {
            println!("{path}: ok");
        } else {
            failed = true;
            println!("{path}: {} error(s)", report.errors.len());
        }
    }
    if warnings > 0 {
        println!("{warnings} warning(s)");
    }
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn run_generate(args: &[String]) -> ExitCode {
    let mut paths: Vec<String> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut format: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--out" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("--out needs a path");
                    return ExitCode::from(2);
                };
                out = Some(PathBuf::from(value));
            }
            "--format" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    eprintln!("--format needs rust or markdown");
                    return ExitCode::from(2);
                };
                if value != "rust" && value != "markdown" {
                    eprintln!("--format must be rust or markdown, got {value}");
                    return ExitCode::from(2);
                }
                format = Some(value.clone());
            }
            other => paths.push(other.to_string()),
        }
        index += 1;
    }
    if paths.is_empty() {
        usage();
        return ExitCode::from(2);
    }
    if paths.len() > 1 && out.is_some() {
        eprintln!("--out takes one map; give one file at a time");
        return ExitCode::from(2);
    }

    let mut rendered = Vec::new();
    for path in &paths {
        let report = check_file(Path::new(path));
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        if !report.errors.is_empty() {
            for error in &report.errors {
                eprintln!("error: {error}");
            }
            eprintln!("{path}: not generated, {} error(s)", report.errors.len());
            return ExitCode::from(1);
        }

        let data: Value = match read_map(path) {
            Ok(data) => data,
            Err(error) => {
                eprintln!("error: {error}");
                return ExitCode::from(1);
            }
        };
        let kind = data["kind"].as_str().unwrap_or("shim");
        let mut chosen = format.clone().unwrap_or_else(|| {
            if kind == "inventory" {
                "markdown".to_string()
            } else {
                "rust".to_string()
            }
        });
        if chosen == "rust" && kind == "inventory" {
            eprintln!("warning: {path} is an inventory map; generating Markdown");
            chosen = "markdown".to_string();
        }
        rendered.push(if chosen == "rust" {
            render_rust(&data, path)
        } else {
            render_markdown(&data, path)
        });
    }

    let text = rendered.join("\n");
    match out {
        Some(path) => {
            if let Some(directory) = path.parent() {
                if !directory.as_os_str().is_empty() {
                    if let Err(error) = fs::create_dir_all(directory) {
                        eprintln!("error: {}: {error}", directory.display());
                        return ExitCode::from(1);
                    }
                }
            }
            if let Err(error) = fs::write(&path, text) {
                eprintln!("error: {}: {error}", path.display());
                return ExitCode::from(1);
            }
            println!("wrote {}", path.display());
        }
        None => print!("{text}"),
    }
    ExitCode::SUCCESS
}

fn read_map(path: &str) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{path}: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("{path}: {error}"))
}

// ---- validation ----------------------------------------------------------

struct Report {
    errors: Vec<String>,
    warnings: Vec<String>,
}

impl Report {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            warnings: Vec::new(),
        }
    }
}

fn check_file(path: &Path) -> Report {
    let mut report = Report::new();
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) => {
            report.errors.push(format!("{name}: cannot read: {error}"));
            return report;
        }
    };
    let data: Value = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(error) => {
            report.errors.push(format!("{name}: cannot read: {error}"));
            return report;
        }
    };
    check_map(&mut report, &name, &data);
    report
}

fn check_map(report: &mut Report, file: &str, data: &Value) {
    let Some(map) = data.as_object() else {
        report.errors.push(format!("{file}: top level is not an object"));
        return;
    };

    let allowed = ["kind", "dll", "target", "notes", "functions"];
    let unknown: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if !unknown.is_empty() {
        report
            .errors
            .push(format!("{file}: unknown top-level fields: {unknown:?}"));
    }

    let kind = map.get("kind").and_then(Value::as_str).unwrap_or("");
    if !KINDS.contains(&kind) {
        report
            .errors
            .push(format!("{file}: kind must be one of {:?}, got {kind:?}", KINDS));
    }
    for field in ["dll", "target"] {
        let value = map.get(field).and_then(Value::as_str).unwrap_or("");
        if value.trim().is_empty() {
            report.errors.push(format!("{file}: {field} is required"));
        }
    }

    let Some(functions) = map.get("functions").and_then(Value::as_array) else {
        report
            .errors
            .push(format!("{file}: functions must be a non-empty array"));
        return;
    };
    if functions.is_empty() {
        report
            .errors
            .push(format!("{file}: functions must be a non-empty array"));
        return;
    }

    let dll = map.get("dll").and_then(Value::as_str).unwrap_or("");
    let mut seen: Vec<(String, Option<String>, String)> = Vec::new();
    for (index, function) in functions.iter().enumerate() {
        let label = function
            .get("name")
            .and_then(Value::as_str)
            .map(|name| name.to_string())
            .unwrap_or_else(|| format!("#{index}"));
        check_function(report, &format!("{file}[{label}]"), kind, function, dll, &mut seen);
    }
}

fn check_function(
    report: &mut Report,
    where_: &str,
    kind: &str,
    function: &Value,
    file_dll: &str,
    seen: &mut Vec<(String, Option<String>, String)>,
) {
    let Some(entry) = function.as_object() else {
        report.errors.push(format!("{where_}: entry is not an object"));
        return;
    };
    let Some(name) = entry.get("name").and_then(Value::as_str) else {
        report
            .errors
            .push(format!("{where_}: invalid or missing name: {function}"));
        return;
    };
    if !is_identifier(name) {
        report
            .errors
            .push(format!("{where_}: invalid or missing name: {name:?}"));
        return;
    }

    let dll = entry
        .get("dll")
        .and_then(Value::as_str)
        .unwrap_or(file_dll)
        .to_string();
    let interface = entry.get("object").and_then(Value::as_str).map(str::to_string);
    let key = (dll.clone(), interface.clone(), name.to_string());
    if seen.contains(&key) {
        report
            .errors
            .push(format!("{where_}: duplicate entry for {dll}!{name}"));
    }
    seen.push(key);

    let described = entry
        .get("description")
        .and_then(Value::as_str)
        .is_some_and(|text| !text.trim().is_empty());
    if !described {
        report
            .errors
            .push(format!("{where_}: description is required and must not be empty"));
    }

    if let Some(rust_name) = entry.get("rust_name").and_then(Value::as_str) {
        if !is_snake_case(rust_name) {
            report
                .errors
                .push(format!("{where_}: rust_name is not a snake_case identifier: {rust_name:?}"));
        }
    }

    if let Some(value) = entry.get("arg_bytes") {
        match value.as_u64() {
            Some(arg_bytes) => {
                if let Some(args) = entry.get("args").and_then(Value::as_array) {
                    if !args.is_empty() {
                        let expected = 4 * args.len() as u64;
                        if expected != arg_bytes {
                            report.warnings.push(format!(
                                "{where_}: arg_bytes {arg_bytes} does not match {} four-byte \
                                 arguments ({expected}); check the signature if this is not \
                                 deliberate",
                                args.len()
                            ));
                        }
                    }
                }
            }
            None => report
                .errors
                .push(format!("{where_}: arg_bytes must be a non-negative integer")),
        }
    }

    if !entry.contains_key("evidence") {
        report.warnings.push(format!(
            "{where_}: no evidence recorded; this stub cannot be re-checked later"
        ));
    }

    if kind == "shim" {
        match entry.get("stub") {
            Some(stub) => check_stub(report, where_, stub),
            None if interface.is_none() => report
                .errors
                .push(format!("{where_}: shim map entry needs a stub")),
            None => {}
        }
    } else {
        let status = entry.get("status").and_then(Value::as_str).unwrap_or("");
        if !STATUSES.contains(&status) {
            report.errors.push(format!(
                "{where_}: inventory entry needs a valid status, got {status:?}"
            ));
        }
        if status == "replaced"
            && entry
                .get("replaced_by")
                .and_then(Value::as_str)
                .map_or(true, |text| text.is_empty())
        {
            report
                .errors
                .push(format!("{where_}: status 'replaced' needs replaced_by"));
        }
    }
}

fn check_stub(report: &mut Report, where_: &str, stub: &Value) {
    let Some(fields) = stub.as_object() else {
        report.errors.push(format!("{where_}: stub is not an object"));
        return;
    };
    let kind = fields.get("kind").and_then(Value::as_str).unwrap_or("");
    if !STUB_KINDS.contains(&kind) {
        report.errors.push(format!(
            "{where_}: stub kind must be one of {:?}, got {kind:?}",
            STUB_KINDS
        ));
        return;
    }
    if kind == "return" && !fields.get("value").is_some_and(Value::is_string) {
        report
            .errors
            .push(format!("{where_}: stub kind 'return' needs a string value"));
    }
    if kind == "custom" && !fields.get("body").is_some_and(Value::is_string) {
        report
            .errors
            .push(format!("{where_}: stub kind 'custom' needs a string body"));
    }
    let allowed = ["kind", "value", "body", "reason"];
    let unknown: Vec<&str> = fields
        .keys()
        .map(String::as_str)
        .filter(|key| !allowed.contains(key))
        .collect();
    if !unknown.is_empty() {
        report
            .errors
            .push(format!("{where_}: unknown stub fields: {unknown:?}"));
    }
}

fn is_identifier(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn is_snake_case(name: &str) -> bool {
    let mut characters = name.chars();
    match characters.next() {
        Some(first) if first.is_ascii_lowercase() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_lowercase() || character.is_ascii_digit() || character == '_')
}

// ---- rendering -----------------------------------------------------------

fn snake_case(name: &str) -> String {
    let characters: Vec<char> = name.chars().collect();
    let mut out = String::new();
    for (index, character) in characters.iter().enumerate() {
        if character.is_uppercase() && index > 0 {
            let previous = characters[index - 1];
            let next_lower = characters
                .get(index + 1)
                .is_some_and(|next| next.is_lowercase());
            if previous.is_lowercase()
                || previous.is_ascii_digit()
                || (previous.is_uppercase() && next_lower)
            {
                out.push('_');
            }
        }
        out.extend(character.to_lowercase());
    }
    out
}

fn arg_bytes_of(function: &Map<String, Value>) -> u64 {
    if let Some(value) = function.get("arg_bytes").and_then(Value::as_u64) {
        return value;
    }
    function
        .get("args")
        .and_then(Value::as_array)
        .map_or(0, |args| 4 * args.len() as u64)
}

fn uses(body: &str, word: &str) -> bool {
    let bytes = body.as_bytes();
    let needle = word.as_bytes();
    let mut start = 0;
    while let Some(found) = body[start..].find(word) {
        let at = start + found;
        let before = at == 0 || !is_word_byte(bytes[at - 1]);
        let after = at + needle.len() >= bytes.len() || !is_word_byte(bytes[at + needle.len()]);
        if before && after {
            return true;
        }
        start = at + needle.len();
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn file_name(path: &str) -> String {
    Path::new(path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string())
}

fn render_rust(data: &Value, source: &str) -> String {
    let dll = data["dll"].as_str().unwrap_or("");
    let target = data["target"].as_str().unwrap_or("");
    let functions = data["functions"].as_array().cloned().unwrap_or_default();
    let imported: Vec<&Value> = functions
        .iter()
        .filter(|function| function.get("object").is_none())
        .collect();
    let dispatched: Vec<&Value> = functions
        .iter()
        .filter(|function| function.get("object").is_some())
        .collect();

    let mut out: Vec<String> = vec![
        format!("// Generated by win32-shim-kit from {}.", file_name(source)),
        format!("// Target: {target}   DLL: {dll}"),
        "// Do not edit: change the map and regenerate.".to_string(),
        String::new(),
        "use crate::shim::{Action, Binding, Call, Host};".to_string(),
        "use crate::Result;".to_string(),
        String::new(),
        "use Action::Return;".to_string(),
        String::new(),
    ];

    if !dispatched.is_empty() {
        out.push("// Reached through a COM vtable, dispatched by the host rather than an".into());
        out.push("// import slot. Listed here so the map stays the whole picture:".into());
        for function in &dispatched {
            let name = function["name"].as_str().unwrap_or("");
            let rust_name = function
                .get("rust_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| snake_case(name));
            let object = function["object"].as_str().unwrap_or("");
            out.push(format!("//   {object}!{name}  ({rust_name})"));
        }
        out.push(String::new());
    }

    if imported.is_empty() {
        out.push("// Every entry in this map is a COM method; this module has no imports.".into());
        out.push(String::new());
        return out.join("\n") + "\n";
    }

    out.push("pub const SHIMS: &[Binding] = &[".into());
    let mut signatures: Vec<(String, Vec<String>)> = Vec::new();
    for function in &imported {
        let entry = function.as_object().expect("validated object");
        let name = entry["name"].as_str().unwrap_or("");
        let rust_name = entry
            .get("rust_name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| snake_case(name));
        let block = rust_function(entry, name, dll, &rust_name);
        signatures.push((rust_name.clone(), block));
        out.push(format!(
            "    Binding {{ dll: \"{dll}\", name: \"{name}\", arg_bytes: {}, shim: {rust_name} }},",
            arg_bytes_of(entry)
        ));
    }
    out.push("];".into());
    out.push(String::new());

    let mut seen: Vec<String> = Vec::new();
    for (name, block) in signatures {
        if seen.contains(&name) {
            continue;
        }
        seen.push(name);
        out.extend(block);
    }

    let mut text = out.join("\n");
    while text.ends_with('\n') {
        text.pop();
    }
    text + "\n"
}

fn rust_function(
    entry: &Map<String, Value>,
    name: &str,
    dll: &str,
    rust_name: &str,
) -> Vec<String> {
    let stub = entry.get("stub").cloned().unwrap_or_else(|| {
        serde_json::json!({ "kind": "unimplemented", "reason": "not described" })
    });
    let kind = stub["kind"].as_str().unwrap_or("unimplemented");
    let body: Vec<String> = match kind {
        "return" => vec![format!("Ok(Return({}))", stub["value"].as_str().unwrap_or("0"))],
        "noop" => vec!["Ok(Return(1))".to_string()],
        "custom" => {
            let text = stub["body"].as_str().unwrap_or("");
            let mut lines: Vec<String> = text.lines().map(|line| line.trim_end().to_string()).collect();
            if lines.last().is_some_and(|line| line.is_empty()) {
                lines.pop();
            }
            lines
        }
        _ => {
            let reason = stub
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("{name} from {dll}"));
            vec![format!("Err(\"{reason}\".to_string())")]
        }
    };

    let source = body.join("\n");
    let host = if uses(&source, "host") { "host" } else { "_host" };
    let call = if uses(&source, "call") { "call" } else { "_call" };

    let description = entry["description"].as_str().unwrap_or("");
    let mut lines = vec![
        format!("/// {name} -- {description}"),
        format!("fn {rust_name}({host}: &mut Host, {call}: &mut Call<'_>) -> Result<Action> {{"),
    ];
    for line in &body {
        if line.trim().is_empty() {
            lines.push(String::new());
        } else {
            lines.push(format!("    {line}"));
        }
    }
    lines.push("}".to_string());
    lines.push(String::new());
    lines
}

fn render_markdown(data: &Value, source: &str) -> String {
    let dll = data["dll"].as_str().unwrap_or("");
    let target = data["target"].as_str().unwrap_or("");
    let mut out = vec![
        format!("# {dll} -- {target}"),
        String::new(),
        format!("Generated by win32-shim-kit from `{}`.", file_name(source)),
    ];
    if let Some(notes) = data.get("notes").and_then(Value::as_str) {
        if !notes.is_empty() {
            out.push(String::new());
            out.push(notes.to_string());
        }
    }
    let functions = data["functions"].as_array().cloned().unwrap_or_default();
    if data["kind"].as_str() == Some("inventory") {
        out.push(String::new());
        out.push("| Function | Status | Replaced by | Why |".into());
        out.push("| --- | --- | --- | --- |".into());
        for function in functions {
            let name = function["name"].as_str().unwrap_or("");
            let status = function.get("status").and_then(Value::as_str).unwrap_or("-");
            let replaced = function.get("replaced_by").and_then(Value::as_str).unwrap_or("-");
            let description = function["description"].as_str().unwrap_or("").replace('|', "\\|");
            out.push(format!("| `{name}` | {status} | {replaced} | {description} |"));
        }
    } else {
        out.push(String::new());
        out.push("| Function | Stub | Object | Why |".into());
        out.push("| --- | --- | --- | --- |".into());
        for function in functions {
            let name = function["name"].as_str().unwrap_or("");
            let stub = function
                .get("stub")
                .and_then(|stub| stub.get("kind"))
                .and_then(Value::as_str)
                .unwrap_or("-");
            let interface = function.get("object").and_then(Value::as_str).unwrap_or("import");
            let description = function["description"].as_str().unwrap_or("").replace('|', "\\|");
            out.push(format!("| `{name}` | {stub} | {interface} | {description} |"));
        }
    }
    out.join("\n") + "\n"
}
