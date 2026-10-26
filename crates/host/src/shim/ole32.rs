//! The `ole32.dll` calls a game makes even when it does not use COM objects.
//!
//! A DirectX-era program initialises COM at start-up on the way to something
//! else (a clipboard format, a shell helper, a sound library), and this host has
//! no COM. Refusing `CoCreateInstance` with `E_NOINTERFACE` is the truthful
//! answer -- the object does not exist -- and it is the answer a caller that
//! treats the object as optional already handles, unlike a plausible pointer to
//! nothing.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const OLE32: &str = "ole32.dll";

/// `E_NOINTERFACE`: the class is not registered in this machine.
const E_NOINTERFACE: u32 = 0x8000_4002;

pub const SHIMS: &[Binding] = &[
    Binding { dll: OLE32, name: "CoCreateInstance", arg_bytes: 20, shim: co_create_instance },
    Binding { dll: OLE32, name: "CoInitialize", arg_bytes: 4, shim: co_initialize },
    Binding { dll: OLE32, name: "CoUninitialize", arg_bytes: 0, shim: co_uninitialize },
];

fn co_initialize(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // S_FALSE: the apartment was already initialised, which is what a program
    // that calls this twice is told.
    Ok(Return(1))
}

fn co_uninitialize(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn co_create_instance(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // A filter graph is the one class this host implements; everything else is
    // refused below, which is what a machine with no COM object registered says.
    if let Some(action) = crate::shim::quartz::create_instance(host, call)? {
        return Ok(action);
    }
    let destination = call.arg(4)?;
    if destination != 0 {
        call.write_u32(destination, 0)?;
    }
    Ok(Return(E_NOINTERFACE))
}
