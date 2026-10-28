//! The `ole32.dll` calls a game makes even when it does not use COM objects.
//!
//! A DirectX-era program initialises COM at start-up on the way to something
//! else (a clipboard format, a shell helper, a sound library), and this host has
//! no COM. Refusing `CoCreateInstance` with `E_NOINTERFACE` is the truthful
//! answer -- the object does not exist -- and it is the answer a caller that
//! treats the object as optional already handles, unlike a plausible pointer to
//! nothing.
//!
//! The COM task allocator is the one piece of COM a caller can rely on without
//! an object existing, because the allocating side is this process:
//! `CoTaskMemAlloc` takes a block from the same heap `HeapAlloc` does and
//! `CoTaskMemFree` gives it back. A block handed to the shell or to a sound
//! library is therefore a pointer this host can read.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const OLE32: &str = "ole32.dll";

/// `E_NOINTERFACE`: the class is not registered in this machine.
const E_NOINTERFACE: u32 = 0x8000_4002;

pub const SHIMS: &[Binding] = &[
    Binding { dll: OLE32, name: "CoCreateInstance", arg_bytes: 20, shim: co_create_instance },
    Binding { dll: OLE32, name: "CoInitialize", arg_bytes: 4, shim: co_initialize },
    Binding { dll: OLE32, name: "CoTaskMemAlloc", arg_bytes: 4, shim: co_task_mem_alloc },
    Binding { dll: OLE32, name: "CoTaskMemFree", arg_bytes: 4, shim: co_task_mem_free },
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

fn co_task_mem_alloc(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // The task allocator's contract is a zeroed block, because callers hand the
    // result straight to an API that fills it. Asking for nothing still answers
    // a pointer, which is what Windows does; the heap refuses impossible sizes
    // and that is the only case that reads as failure.
    let size = call.arg(0)?;
    match host.allocate(size.max(1), 4) {
        Ok(block) => {
            if size != 0 {
                call.zero(block, size)?;
            }
            Ok(Return(block))
        }
        Err(_) => Ok(Return(0)),
    }
}

fn co_task_mem_free(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let block = call.arg(0)?;
    if block != 0 {
        host.release(block);
    }
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
