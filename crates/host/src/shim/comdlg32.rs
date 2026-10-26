//! `COMDLG32.dll`: the common file dialogs, which a page cannot show.
//!
//! A game opens a picker to load a recording, a map or a mod package. There is no
//! dialog here, and the truthful answer is the one a user gives by pressing
//! Cancel: `0`. The notice names the dialog's own title and default file, because
//! a game that asks and is told "cancelled" looks like a bug from the outside --
//! and a caller that loops until it gets a file would loop for ever without it.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const COMDLG32: &str = "COMDLG32.dll";

/// Offsets in `OPENFILENAMEA`, which is what both calls are given.
const OFN_FILE: u32 = 28;
const OFN_TITLE: u32 = 48;

pub const SHIMS: &[Binding] = &[
    Binding { dll: COMDLG32, name: "GetOpenFileNameA", arg_bytes: 4, shim: get_open_file_name },
    Binding { dll: COMDLG32, name: "GetSaveFileNameA", arg_bytes: 4, shim: get_save_file_name },
];

/// `GetOpenFileNameA(&OPENFILENAMEA)`: cancelled.
fn get_open_file_name(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    cancelled(host, call, "GetOpenFileNameA")
}

/// `GetSaveFileNameA(&OPENFILENAMEA)`: cancelled.
fn get_save_file_name(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    cancelled(host, call, "GetSaveFileNameA")
}

fn dialog_string(call: &Call<'_>, address: u32) -> Result<String> {
    if address == 0 {
        return Ok(String::new());
    }
    call.memory.read_cstr(address)
}

fn cancelled(host: &mut Host, call: &mut Call<'_>, method: &str) -> Result<Action> {
    if host
        .notices
        .iter()
        .any(|notice| notice.starts_with(method))
    {
        return Ok(Return(0));
    }
    let structure = call.arg(0)?;
    let title = if structure == 0 {
        String::new()
    } else {
        dialog_string(call, call.memory.read_u32(structure + OFN_TITLE)?)?
    };
    let file = if structure == 0 {
        String::new()
    } else {
        dialog_string(call, call.memory.read_u32(structure + OFN_FILE)?)?
    };
    host.notice(format!(
        "{method}: this host shows no file dialog, so the request{} was cancelled",
        match (title.is_empty(), file.is_empty()) {
            (false, false) => format!(" for \u{201c}{title}\u{201d} (default name {file})"),
            (false, true) => format!(" for \u{201c}{title}\u{201d}"),
            (true, false) => format!(" (default name {file})"),
            (true, true) => String::new(),
        }
    ));
    Ok(Return(0))
}
