//! `DINPUT.dll`: the input objects the guest creates before anything is drawn.
//!
//! WinMain creates its `IDirectInput` object before it touches DirectDraw, and it
//! has no fallback -- a non-zero `HRESULT` prints a message and the caller returns
//! failure (checked at `0x0043738c`). So the object has to exist for the title
//! screen to be reached at all.
//!
//! The object is a host object: four bytes of vtable pointer in guest memory,
//! pointing at a table of thunks ([`crate::thunks`]). `CreateDevice` hands out an
//! `IDirectInputDevice` whose acquired state is the host's own device state
//! ([`crate::input`]), so a key the page reports is a key the guest polls and the
//! two views cannot drift apart. Only the keyboard and the mouse carry state; a
//! joystick object can still be created but reports nothing, because no page has
//! a stick to offer it and the game treats its absence as optional.
//!
//! The image imports exactly one DirectInput entry point, `DirectInputCreateA`, so
//! that is the only one bound here.

use crate::shim::{Action, Binding, Call, Host};
use crate::thunks;
use crate::Result;

use Action::Return;

const DINPUT: &str = "DINPUT.dll";

/// `DIRECTINPUT_VERSION`, 5.0, which is what the guest asks for.
const DIRECTINPUT_VERSION_5: u32 = 0x0500;

const E_NOINTERFACE: u32 = 0x8000_4002;

pub const SHIMS: &[Binding] = &[Binding {
    dll: DINPUT,
    name: "DirectInputCreateA",
    arg_bytes: 16,
    shim: direct_input_create,
}];

fn direct_input_create(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let version = call.arg(1)?;
    let destination = call.arg(2)?;
    let outer = call.arg(3)?;
    create(host, call, version, destination, outer)
}

fn create(
    host: &mut Host,
    call: &mut Call<'_>,
    version: u32,
    destination: u32,
    outer: u32,
) -> Result<Action> {
    if version < DIRECTINPUT_VERSION_5 {
        // The object exists, but the guest asked for an older interface than this
        // host models; failing is better than handing back a vtable that does not
        // match what it expects.
        host.notice(format!(
            "DirectInputCreateA: version {version:#06x} is older than 5.0, refused"
        ));
        return Ok(Return(0x8000_4003)); // DIERR_OLDDIRECTINPUTVERSION
    }
    if outer != 0 {
        // No aggregation: nothing in the image asks for it, and a lie here would
        // hand the guest an object it does not own.
        return Ok(Return(E_NOINTERFACE));
    }
    if destination == 0 {
        return Ok(Return(0x8007_0057)); // E_INVALIDARG
    }

    let object = thunks::new_object(call.memory, host, thunks::DINPUT)?;
    call.write_u32(destination, object)?;
    host.notice("DirectInput: object created; device state comes from the page".to_string());
    Ok(Return(0))
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
        name: &str,
        arguments: &[u32],
    ) -> Result<Action> {
        let binding = SHIMS
            .iter()
            .find(|binding| binding.name == name)
            .unwrap_or_else(|| panic!("{name} is implemented"));
        let args = STACK_BASE + 0xb000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(args, &bytes).unwrap();
        let mut call = Call {
            memory,
            args,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call)
    }

    #[test]
    fn the_object_is_created_and_points_at_the_thunks() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let destination = STACK_BASE + 0xb100;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInputCreateA",
                &[0x0040_0000, 0x0500, destination, 0]
            )
            .unwrap(),
            Return(0)
        );
        let object = memory.read_u32(destination).unwrap();
        assert!(object > 0);
        let table = memory.read_u32(object).unwrap();
        let create_device = memory.read_u32(table + 12).unwrap();
        assert_eq!(thunks::at(create_device), Some((thunks::DINPUT, 3)));
        assert_eq!(host.notices.len(), 1);
    }

    #[test]
    fn an_old_version_is_refused_rather_than_mismatched() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInputCreateA",
                &[0x0040_0000, 0x0300, STACK_BASE + 0xb200, 0]
            )
            .unwrap(),
            Return(0x8000_4003)
        );
    }

    #[test]
    fn aggregation_and_a_missing_out_parameter_are_errors() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInputCreateA",
                &[0x0040_0000, 0x0500, STACK_BASE + 0xb300, 0x1234]
            )
            .unwrap(),
            Return(E_NOINTERFACE)
        );
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInputCreateA",
                &[0x0040_0000, 0x0500, 0, 0]
            )
            .unwrap(),
            Return(0x8007_0057)
        );
    }
}
