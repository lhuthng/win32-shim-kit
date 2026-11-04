//! Dialogs (`USER32.dll`): the window a template describes, and the controls a
//! guest names by id.
//!
//! This host has no system controls to draw -- no edit field, no push button, no
//! static text -- so a dialog is *not* a bitmap of its template. What is modelled
//! is what a guest can observe and act on: the dialog's window handle, a child
//! window handle per control id, and that control's text and enabled state. A
//! game that fills a field with `SendDlgItemMessageA(WM_SETTEXT)`, greys the OK
//! button with `EnableWindow`, and reads the field back gets exactly what it
//! wrote, which is the part of a dialog a port has to keep. What it cannot get is
//! a control painted on the screen, because there is nothing to paint it with --
//! the game draws its own screens, and a dialog that blocks the guest to be filled
//! in is the one thing this machine does not have.
//!
//! `IsDialogMessageA` therefore answers zero: the guest's own loop keeps its keys.
//! That is the honest answer, not a stub: a host that claimed to have handled a
//! key would swallow the keyboard the game runs on.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const USER32: &str = "USER32.dll";

/// The window class a dialog is created from, which is the one Windows uses for
/// every dialog whatever its template says.
const DIALOG_CLASS: &str = "#32770";

/// The dialog messages this host answers with real state, and the text ones a
/// control here can honour.
const WM_SETFOCUS: u32 = 0x0007;
const WM_ENABLE: u32 = 0x000a;
const WM_SETTEXT: u32 = 0x000c;
const WM_GETTEXT: u32 = 0x000d;
const WM_GETTEXTLENGTH: u32 = 0x000e;

pub const SHIMS: &[Binding] = &[
    Binding { dll: USER32, name: "CreateDialogIndirectParamA", arg_bytes: 20, shim: create_dialog_indirect },
    Binding { dll: USER32, name: "EndDialog", arg_bytes: 8, shim: end_dialog },
    Binding { dll: USER32, name: "DefDlgProcA", arg_bytes: 16, shim: def_dlg_proc },
    Binding { dll: USER32, name: "GetDlgItem", arg_bytes: 8, shim: get_dlg_item },
    Binding { dll: USER32, name: "GetDlgCtrlID", arg_bytes: 4, shim: get_dlg_ctrl_id },
    Binding { dll: USER32, name: "SendDlgItemMessageA", arg_bytes: 20, shim: send_dlg_item_message },
    Binding { dll: USER32, name: "GetNextDlgTabItem", arg_bytes: 12, shim: get_next_dlg_tab_item },
    Binding { dll: USER32, name: "IsDialogMessageA", arg_bytes: 8, shim: is_dialog_message },
    Binding { dll: USER32, name: "MapDialogRect", arg_bytes: 8, shim: map_dialog_rect },
];

/// `CreateDialogIndirectParamA(hInstance, template, parent, proc, param)`: the
/// dialog's window, whose controls appear as the guest names them.
///
/// Only the template's header is read -- its style, which decides whether the
/// dialog is visible and whether it is a child -- because the item templates
/// describe pixels this host has nothing to draw them with. The item ids still
/// reach the guest, through `GetDlgItem`, the first time it asks for one.
fn create_dialog_indirect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let template = call.arg(1)?;
    let parent = call.arg(2)?;
    let proc_address = call.arg(3)?;
    if template == 0 || proc_address == 0 {
        // Windows fails the creation without a template or a procedure, and a
        // guest checks the answer before using the handle.
        return Ok(Return(0));
    }
    let style = call.memory.read_u32(template)?;
    let (width, height) = host.windows.screen();
    let window = host.windows.add_window(
        DIALOG_CLASS.to_string(),
        proc_address,
        style,
        [0, 0, width, height],
    );
    if parent != 0 {
        host.windows.set_parent(window, parent);
    }
    host.windows.add_dialog(window);
    Ok(Return(window))
}

/// `GetDlgItem(hDlg, id)`: the control's window handle, made the first time the
/// guest names its id.
///
/// A guest uses the handle for `EnableWindow`, `SetWindowTextA` and `GetWindowRect`
/// as readily as for the `SendDlgItemMessageA` pair, so the handle has to exist
/// and stay the same number across calls.
fn get_dlg_item(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dialog = call.arg(0)?;
    let id = call.arg(1)?;
    Ok(Return(resolve_item(host, dialog, id).unwrap_or(0)))
}

/// The child handle for a control id, creating it when the guest first asks.
///
/// `None` means the handle is not a dialog this host created, which is the one
/// answer that has to stay zero: a guest that passes its main window here must
/// not get a control invented for it.
fn resolve_item(host: &mut Host, dialog: u32, id: u32) -> Option<u32> {
    if let Some(item) = host.windows.dialog_item(dialog, id) {
        return Some(item.handle);
    }
    if !host.windows.dialogs.contains_key(&dialog) {
        return None;
    }
    // No template was read, so the control starts with no text and nowhere in
    // particular: the guest's first `WM_SETTEXT` gives it both a label and
    // something to answer a read with.
    Some(host.windows.add_dialog_item(dialog, id, String::new(), [0, 0, 0, 0]))
}

/// `GetDlgCtrlID(hwnd)`: the id a control was created with, which a guest reads
/// to tell which button sent it a `WM_COMMAND`.
fn get_dlg_ctrl_id(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let found = host
        .windows
        .dialogs
        .keys()
        .find_map(|dialog| host.windows.dialog_item_id(*dialog, handle));
    Ok(Return(found.unwrap_or(0)))
}

/// `SendDlgItemMessageA(hDlg, id, msg, wParam, lParam)`: a message to one
/// control.
///
/// The text messages are answered for real, because a game stores its settings in
/// dialog fields and reads them back to write its configuration: a host that
/// swallowed `WM_SETTEXT` would save an empty file. Everything else a control
/// would answer with its own painted state -- how many lines an edit field shows,
/// whether a check box is ticked -- answers zero.
fn send_dlg_item_message(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dialog = call.arg(0)?;
    let id = call.arg(1)?;
    let message = call.arg(2)?;
    let wparam = call.arg(3)?;
    let lparam = call.arg(4)?;
    let Some(handle) = resolve_item(host, dialog, id) else {
        return Ok(Return(0));
    };
    match message {
        WM_SETTEXT => {
            let text = call.memory.read_cstr(lparam)?;
            if let Some(item) = host.windows.dialog_item_mut(dialog, id) {
                item.text = text.clone();
            }
            // The control is a window too, so `GetWindowTextA` on the handle the
            // guest got from `GetDlgItem` reads the same string back.
            host.windows.set_text(handle, text);
            Ok(Return(1))
        }
        WM_GETTEXT => {
            if lparam == 0 || wparam == 0 {
                return Ok(Return(0));
            }
            let text = host
                .windows
                .dialog_item(dialog, id)
                .map(|item| item.text.clone())
                .unwrap_or_default();
            let limit = wparam as usize;
            let mut bytes = text.as_bytes().to_vec();
            bytes.truncate(limit.saturating_sub(1));
            bytes.push(0);
            let copied = bytes.len() as u32 - 1;
            call.write(lparam, &bytes)?;
            Ok(Return(copied))
        }
        WM_GETTEXTLENGTH => {
            let length = host
                .windows
                .dialog_item(dialog, id)
                .map_or(0, |item| item.text.len() as u32);
            Ok(Return(length))
        }
        WM_ENABLE => {
            let enabled = wparam != 0;
            if let Some(item) = host.windows.dialog_item_mut(dialog, id) {
                item.enabled = enabled;
            }
            // `EnableWindow` on the same handle and this message are the same act
            // as far as the guest can tell, so they move the same state.
            host.windows.set_enabled(handle, enabled);
            Ok(Return(0))
        }
        WM_SETFOCUS => {
            host.windows.focus = handle;
            Ok(Return(0))
        }
        _ => Ok(Return(0)),
    }
}

/// `GetNextDlgTabItem(hDlg, hCtl, bPrevious)`: the control the tab key moves to,
/// wrapping at either end.
///
/// A guest uses this to place the keyboard itself when it draws its own dialog
/// body, so the order has to be the template's -- which is the order the controls
/// were first named here.
fn get_next_dlg_tab_item(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dialog = call.arg(0)?;
    let current = call.arg(1)?;
    let previous = call.arg(2)? != 0;
    let Some(entry) = host.windows.dialogs.get(&dialog) else {
        return Ok(Return(0));
    };
    let handles: Vec<u32> = entry.items.iter().map(|(_, item)| item.handle).collect();
    if handles.is_empty() {
        return Ok(Return(0));
    }
    let Some(index) = handles.iter().position(|handle| *handle == current) else {
        // A handle that is not a control here moves to the first one, which is
        // what Windows does with the dialog's own handle.
        return Ok(Return(handles[0]));
    };
    let count = handles.len();
    let next = if previous {
        (index + count - 1) % count
    } else {
        (index + 1) % count
    };
    Ok(Return(handles[next]))
}

/// `EndDialog(hDlg, result)`: the dialog is finished, and the guest's own routine
/// carries on with the result.
///
/// The result is recorded as a notice rather than handed anywhere: this host does
/// not run a modal loop that could return it, so a trace is where it survives --
/// and it says which button a player pressed, which is what a port needs.
fn end_dialog(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let dialog = call.arg(0)?;
    let result = call.arg(1)?;
    let known = host.windows.dialogs.remove(&dialog).is_some();
    host.notice(format!(
        "EndDialog: dialog {dialog:#010x} closed with result {result} ({})",
        if known { "a dialog this host created" } else { "a handle it did not know" }
    ));
    Ok(Return(1))
}

/// `DefDlgProcA(hwnd, msg, wParam, lParam)`: the default processing of a dialog
/// message. A guest that passes a message its own procedure does not handle gets
/// zero, which is what a dialog's default procedure answers for the messages this
/// host has nothing to do about.
fn def_dlg_proc(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `IsDialogMessageA(hDlg, lpMsg)`: whether the dialog took the message.
///
/// Zero. This host has no system dialog keyboard navigation, and claiming a key
/// would take it away from the guest's own loop -- a game that checks this before
/// handling its keys would stop answering the keyboard entirely.
fn is_dialog_message(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `MapDialogRect(hDlg, lpRect)`: dialog units to pixels, which is the same
/// rectangle here because the base unit of a host with no system font is a pixel.
fn map_dialog_rect(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}
