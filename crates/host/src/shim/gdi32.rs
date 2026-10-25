//! The `GDI32.dll` subset the boot needs.
//!
//! For now that is one call: `GetStockObject`, which the window class uses for
//! its background brush. The handle it returns is opaque to the guest -- it is
//! passed straight back to `SelectObject`/`DeleteObject` and never inspected --
//! so this module hands out one stable handle per stock index and nothing else.
//! The calls that would actually paint (`SelectObject`, `SetBkMode`,
//! `TextOutA`, `BitBlt`, ...) belong to the M3 text and menu work, together with
//! the font the image carries; they have no shim yet, so the session stops and
//! names the first one the guest reaches.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const GDI32: &str = "GDI32.dll";

const FIRST_OBJECT: u32 = 0x0002_0000;

/// The highest stock index on 32-bit Windows (`DC_PEN`); anything above it is
/// not a stock object and answers null, as it does on Windows.
const LAST_STOCK_INDEX: u32 = 19;

pub const SHIMS: &[Binding] = &[
    Binding { dll: GDI32, name: "GetStockObject", arg_bytes: 4, shim: get_stock_object },
];

fn get_stock_object(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let index = call.arg(0)?;
    if index > LAST_STOCK_INDEX {
        return Ok(Return(0));
    }
    Ok(Return(FIRST_OBJECT + index * 4))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    fn call_shim(
        host: &mut Host,
        memory: &mut GuestMemory,
        arguments: &[u32],
    ) -> Result<Action> {
        let args = STACK_BASE + 0x4000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: 4,
        };
        get_stock_object(host, &mut call)
    }

    #[test]
    fn every_stock_object_gets_its_own_non_null_handle() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let white = call_shim(&mut host, &mut memory, &[0]).unwrap();
        let black = call_shim(&mut host, &mut memory, &[4]).unwrap();
        assert_eq!(white, Return(FIRST_OBJECT));
        assert_eq!(black, Return(FIRST_OBJECT + 16));
        assert_ne!(white, black);
    }

    #[test]
    fn an_index_that_is_not_a_stock_object_is_null() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, &[LAST_STOCK_INDEX + 1]).unwrap(),
            Return(0)
        );
    }
}
