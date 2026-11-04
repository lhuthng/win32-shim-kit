//! The `COMCTL32.dll` subset a boot reaches: the common-controls initialiser,
//! the property-sheet entry points, and the one image list a guest destroys.
//!
//! `InitCommonControls` arrives here by **ordinal**, as `#17`. That is the
//! version-5 export table's ordinal for it (the function is also exported by
//! name from later builds), it takes no arguments and answers nothing, and it
//! exists so a program can tell Windows to load the common controls before it
//! uses a property sheet. There is nothing to load here: a control in this host
//! is state the guest reads back, not a window the system draws.
//!
//! A property sheet is the one thing in this file that cannot be served
//! truthfully. `PropertySheetA` runs a modal dialog of its own, and this host has
//! no system controls to put in it, so the sheet reports that the user cancelled
//! -- which is the answer a game already handles, because that is what pressing
//! Escape returns. The notice says so, because a player who opens the options
//! dialog and sees nothing happen deserves to know why.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const COMCTL32: &str = "COMCTL32.dll";

/// Handles for property-sheet pages, above every other handle this host makes.
const FIRST_PAGE: u32 = 0x0005_0000;

pub const SHIMS: &[Binding] = &[
    // `InitCommonControls()`: load the control classes. Nothing to load.
    Binding { dll: COMCTL32, name: "#17", arg_bytes: 0, shim: init_common_controls },
    Binding { dll: COMCTL32, name: "InitCommonControls", arg_bytes: 0, shim: init_common_controls },
    Binding {
        dll: COMCTL32,
        name: "CreatePropertySheetPageA",
        arg_bytes: 4,
        shim: create_property_sheet_page,
    },
    Binding {
        dll: COMCTL32,
        name: "DestroyPropertySheetPage",
        arg_bytes: 4,
        shim: destroy_property_sheet_page,
    },
    Binding {
        dll: COMCTL32,
        name: "ImageList_Destroy",
        arg_bytes: 4,
        shim: image_list_destroy,
    },
    Binding { dll: COMCTL32, name: "PropertySheetA", arg_bytes: 4, shim: property_sheet },
];

/// `InitCommonControls()`: the guest is about to use a common control.
///
/// The call exists so that a Windows build can load the control classes from
/// the DLL; this host's controls are its own state, so there is nothing to do
/// and nothing to answer.
fn init_common_controls(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `CreatePropertySheetPageA(psp)`: a page handle for the sheet header.
///
/// The page description is not read: a property sheet is answered as cancelled
/// below, so the page is a handle a guest can put in a header and destroy again,
/// and nothing in it is drawn.
fn create_property_sheet_page(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let description = call.arg(0)?;
    if description == 0 {
        // Windows fails the call rather than making a page with nothing in it.
        return Ok(Return(0));
    }
    host.comctl_pages += 1;
    Ok(Return(FIRST_PAGE + host.comctl_pages * 4))
}

/// `DestroyPropertySheetPage(page)`: answers that the page is gone.
fn destroy_property_sheet_page(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from(call.arg(0)? != 0)))
}

/// `ImageList_Destroy(list)`: the list held icons for a control this host does
/// not draw, so destroying it is all a guest can observe.
fn image_list_destroy(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from(call.arg(0)? != 0)))
}

/// `PropertySheetA(header)`: reports the sheet cancelled.
///
/// A property sheet is a modal dialog the common-controls library runs, with
/// system controls and a tab strip this host has no way to draw. Answering zero
/// is the value the user pressing Escape produces, so a game that opens its
/// options from a menu carries on rather than stopping -- and the notice says
/// once that the sheet was not shown, so a player is not left guessing.
fn property_sheet(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let header = call.arg(0)?;
    if header == 0 {
        return Ok(Return(0xffff_ffff));
    }
    if host.notices.len() < 600 {
        host.notice(
            "PropertySheetA: this host draws no system dialog, so the property sheet reported \
             cancel (its controls are not implemented)"
                .to_string(),
        );
    }
    Ok(Return(0))
}
