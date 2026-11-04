//! `WINSPOOL.DRV`: the three printing calls a game's print dialog makes.
//!
//! This host has no printers, and saying so is the honest answer rather than a
//! stub that pretends: `OpenPrinterA` fails with the error Windows uses for a
//! name it cannot find, so a print dialog reports that there is nothing to print
//! to and the game carries on. The alternative -- answering a handle -- would put
//! the guest into a spooling path whose next step this host cannot serve at all.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const WINSPOOL: &str = "WINSPOOL.DRV";

/// `ERROR_INVALID_PRINTER_NAME`, which is what a machine with no printers
/// installed answers.
const ERROR_INVALID_PRINTER_NAME: u32 = 1801;

pub const SHIMS: &[Binding] = &[
    Binding { dll: WINSPOOL, name: "OpenPrinterA", arg_bytes: 12, shim: open_printer },
    Binding { dll: WINSPOOL, name: "ClosePrinter", arg_bytes: 4, shim: close_printer },
    Binding {
        dll: WINSPOOL,
        name: "DocumentPropertiesA",
        arg_bytes: 24,
        shim: document_properties,
    },
];

/// `OpenPrinterA(name, phPrinter, defaults)`: no printer answers in the negative,
/// with the error a guest can show.
fn open_printer(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(1)?;
    if destination != 0 {
        // A failed open leaves the handle slot empty, as Windows does.
        call.write_u32(destination, 0)?;
    }
    host.last_error = ERROR_INVALID_PRINTER_NAME;
    Ok(Return(0))
}

/// `ClosePrinter(printer)`: false, because no handle was ever handed out.
fn close_printer(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `DocumentPropertiesA(...)`: -1, which is what a printer driver that cannot
/// describe its capabilities answers.
///
/// A driver's `DEVMODE` is where page size and orientation live; with no driver
/// there is no `DEVMODE`, and a fabricated one would have the guest print
/// settings it cannot apply.
fn document_properties(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0xffff_ffff))
}
