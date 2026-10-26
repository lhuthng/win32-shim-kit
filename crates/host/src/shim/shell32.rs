//! `SHELL32.dll`: the shell a page does not have.
//!
//! Two kinds of call land here.
//!
//! **Launching.** `ShellExecuteA` opens a document, a URL or another program.
//! There is no shell, no association and no second program, so the answer is
//! `SE_ERR_NOASSOC` (31) -- the failure a machine without an association reports,
//! which a game handles by doing nothing. A notice names the target, because a
//! game that silently fails to open its manual looks like a dead button.
//!
//! **Finding a folder.** `SHGetFolderPathA` and `SHGetSpecialFolderPathA` are how a
//! game asks where its saves go, and a game that saves to `Documents\My Games` or
//! to `%APPDATA%` cannot start without them. This host answers with one synthetic
//! profile, `C:\users\player`, so the paths exist and are stable: the file table
//! resolves them by name like any other, and the page persists what the guest
//! writes there.
//!
//! Dragging is here too, for the programs that link it whether or not they use it:
//! nothing was dropped on the page, so the answer is no files.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const SHELL32: &str = "SHELL32.dll";

/// `ShellExecute` success is any value above 32; below that is an error code.
const SE_ERR_NOASSOC: u32 = 31;

/// `CSIDL_FLAG_CREATE` and `CSIDL_FLAG_DONT_VERIFY`, which are requests about how
/// the folder is obtained, not which folder it is.
const CSIDL_MASK: u32 = 0x00ff;
const E_INVALIDARG: u32 = 0x8007_0057;

/// The one user this machine has, in the shape a Windows path takes.
const HOME: &str = "C:\\users\\player";

pub const SHIMS: &[Binding] = &[
    Binding { dll: SHELL32, name: "ShellExecuteA", arg_bytes: 24, shim: shell_execute },
    Binding { dll: SHELL32, name: "ShellExecuteW", arg_bytes: 24, shim: shell_execute_w },
    Binding { dll: SHELL32, name: "DragQueryFileA", arg_bytes: 16, shim: drag_query_file },
    Binding { dll: SHELL32, name: "DragFinish", arg_bytes: 4, shim: drag_finish },
    Binding { dll: SHELL32, name: "SHGetFolderPathA", arg_bytes: 20, shim: get_folder_path },
    Binding { dll: SHELL32, name: "SHGetFolderPathW", arg_bytes: 20, shim: get_folder_path_w },
    Binding { dll: SHELL32, name: "SHGetSpecialFolderPathA", arg_bytes: 16, shim: get_special_folder_path },
    Binding { dll: SHELL32, name: "SHGetSpecialFolderPathW", arg_bytes: 16, shim: get_special_folder_path_w },
];

/// `ShellExecuteA(hwnd, verb, file, parameters, directory, show)`:
/// `SE_ERR_NOASSOC`.
fn shell_execute(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    refuse(host, call, "ShellExecuteA", false)
}

fn shell_execute_w(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    refuse(host, call, "ShellExecuteW", true)
}

fn refuse(host: &mut Host, call: &mut Call<'_>, method: &str, wide: bool) -> Result<Action> {
    if !host.notices.iter().any(|notice| notice.starts_with(method)) {
        let file = call.arg(2)?;
        let target = if wide {
            call.wstr(file)?
        } else if file == 0 {
            String::new()
        } else {
            call.memory.read_cstr(file)?
        };
        host.notice(format!(
            "{method} {target}: this host launches nothing, so the request failed with SE_ERR_NOASSOC"
        ));
    }
    Ok(Return(SE_ERR_NOASSOC))
}

/// `DragQueryFileA(drop, index, buffer, length)`: nothing was dropped on the page.
fn drag_query_file(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn drag_finish(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `SHGetFolderPathA(hwnd, csidl, token, flags, path)`: `S_OK` and the folder.
fn get_folder_path(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let csidl = call.arg(1)?;
    let path = call.arg(4)?;
    answer_folder(call, path, csidl, false)
}

fn get_folder_path_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let csidl = call.arg(1)?;
    let path = call.arg(4)?;
    answer_folder(call, path, csidl, true)
}

/// `SHGetSpecialFolderPathA(hwnd, path, csidl, create)`: whether it was written.
fn get_special_folder_path(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let csidl = call.arg(2)?;
    let path = call.arg(1)?;
    answer_folder(call, path, csidl, false)
}

fn get_special_folder_path_w(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let csidl = call.arg(2)?;
    let path = call.arg(1)?;
    answer_folder(call, path, csidl, true)
}

/// Writes the folder `csidl` names into `path`, or fails the call when it names
/// one this host has no answer for.
fn answer_folder(call: &mut Call<'_>, path: u32, csidl: u32, wide: bool) -> Result<Action> {
    let Some(folder) = folder_path(csidl & CSIDL_MASK) else {
        return Ok(Return(E_INVALIDARG));
    };
    if path == 0 {
        return Ok(Return(E_INVALIDARG));
    }
    if wide {
        for (index, unit) in folder.encode_utf16().chain([0]).enumerate() {
            call.write_u16(path + index as u32 * 2, unit)?;
        }
    } else {
        let mut bytes = folder.as_bytes().to_vec();
        bytes.push(0);
        call.write(path, &bytes)?;
    }
    // `S_OK`, which is also what one of these returns for success, and the
    // boolean the other one answers with: both are non-zero.
    Ok(Return(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_folder_calls_name_one_synthetic_profile() {
        // A game that saves to Documents or AppData has to be given a path, and
        // every path has to sit under the one profile so the file table resolves
        // them all by name.
        assert_eq!(
            folder_path(0x05).as_deref(),
            Some("C:\\users\\player\\documents")
        );
        assert_eq!(
            folder_path(0x1a).as_deref(),
            Some("C:\\users\\player\\appdata\\roaming")
        );
        assert_eq!(
            folder_path(0x1c).as_deref(),
            Some("C:\\users\\player\\appdata\\local")
        );
        assert_eq!(folder_path(0x28).as_deref(), Some("C:\\users\\player"));
        assert_eq!(folder_path(0x26).as_deref(), Some("C:\\program files"));
        assert_eq!(folder_path(0x0077), None, "an unknown CSIDL is a failure");
    }
}

/// The folder a `CSIDL_*` value names on this machine.
fn folder_path(csidl: u32) -> Option<String> {
    let path = match csidl {
        0x00 => format!("{HOME}\\desktop"),                 // CSIDL_DESKTOP
        0x05 | 0x0c => format!("{HOME}\\documents"),        // CSIDL_PERSONAL, MYDOCUMENTS
        0x0d => format!("{HOME}\\music"),                   // CSIDL_MYMUSIC
        0x0e => format!("{HOME}\\videos"),                  // CSIDL_MYVIDEO
        0x10 => format!("{HOME}\\desktop"),                 // CSIDL_DESKTOPDIRECTORY
        0x15 => format!("{HOME}\\appdata\\roaming\\microsoft\\windows\\templates"),
        0x1a => format!("{HOME}\\appdata\\roaming"),        // CSIDL_APPDATA
        0x1c => format!("{HOME}\\appdata\\local"),          // CSIDL_LOCAL_APPDATA
        0x1e => format!("{HOME}\\appdata\\local\\temp"),    // CSIDL_INTERNET_CACHE
        0x23 => "C:\\programdata".to_string(),              // CSIDL_COMMON_APPDATA
        0x24 => "C:\\windows".to_string(),                  // CSIDL_WINDOWS
        0x25 => "C:\\windows\\system32".to_string(),        // CSIDL_SYSTEM
        0x26 => "C:\\program files".to_string(),            // CSIDL_PROGRAM_FILES
        0x27 => format!("{HOME}\\pictures"),                // CSIDL_MYPICTURES
        0x28 => HOME.to_string(),                           // CSIDL_PROFILE
        _ => return None,
    };
    Some(path)
}
