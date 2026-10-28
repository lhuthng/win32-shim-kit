//! `DINPUT.dll` and `dinput8.dll`: the input objects the guest creates before
//! anything is drawn.
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
//! Two entry points are bound. `DirectInputCreateA` is the one an image imports;
//! `DirectInput8Create` is the one a guest resolves at runtime with
//! `LoadLibraryA("...\dinput8.dll")` + `GetProcAddress`, which is how a
//! DirectInput 8 program loads input when it imports nothing. The device vtables
//! of 7.0 and 8.0 agree through `Initialize` (slot 17), which is every method
//! this host models, so one table answers both.

use crate::shim::{Action, Binding, Call, Host};
use crate::thunks;
use crate::Result;

use Action::Return;

const DINPUT: &str = "DINPUT.dll";
const DINPUT8: &str = "dinput8.dll";

/// `DIRECTINPUT_VERSION` 5.0, the newest interface this host models, and 3.0,
/// which one target asks for.
///
/// DirectInput 3.0's `IDirectInput` and `IDirectInputDevice` carry the same first
/// fourteen vtable entries as 5.0's -- `CreateDevice`, then `Acquire`,
/// `GetDeviceState`, `SetDataFormat` and `SetCooperativeLevel` in the same
/// slots -- so one table answers both. What 3.0 lacks (action mapping, effects)
/// is above slot thirteen, and no caller of it here reaches that far. 8.0's
/// factory shares the first five slots and its device the first eighteen, which
/// is why `DirectInput8Create` can hand out the same objects.
const DIRECTINPUT_VERSION_3: u32 = 0x0300;
const DIRECTINPUT_VERSION_5: u32 = 0x0500;

const E_NOINTERFACE: u32 = 0x8000_4002;

pub const SHIMS: &[Binding] = &[
    Binding {
        dll: DINPUT,
        name: "DirectInputCreateA",
        arg_bytes: 16,
        shim: direct_input_create,
    },
    Binding {
        dll: DINPUT8,
        name: "DirectInput8Create",
        arg_bytes: 20,
        shim: direct_input8_create,
    },
];

fn direct_input_create(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let version = call.arg(1)?;
    let destination = call.arg(2)?;
    let outer = call.arg(3)?;
    create(host, call, version, destination, outer)
}

/// `DirectInput8Create(hinst, dwVersion, riidltf, ppvOut, punkOuter)`: the 8.0
/// factory, whose out parameter is the fourth argument where the 7.0 create's is
/// the third.
fn direct_input8_create(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let version = call.arg(1)?;
    let destination = call.arg(3)?;
    let outer = call.arg(4)?;
    create(host, call, version, destination, outer)
}

fn create(
    host: &mut Host,
    call: &mut Call<'_>,
    version: u32,
    destination: u32,
    outer: u32,
) -> Result<Action> {
    if version < DIRECTINPUT_VERSION_3 {
        // Older than anything this host has a vtable for: failing is better than
        // handing back a table the caller would index past.
        host.notice(format!(
            "DirectInputCreateA: version {version:#06x} is older than 3.0, refused"
        ));
        return Ok(Return(0x8000_4003)); // DIERR_OLDDIRECTINPUTVERSION
    }
    if version < DIRECTINPUT_VERSION_5 {
        host.notice(format!(
            "DirectInputCreateA: version {version:#06x} shares 5.0's first fourteen entries, accepted"
        ));
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
                &[0x0040_0000, 0x0200, STACK_BASE + 0xb200, 0]
            )
            .unwrap(),
            Return(0x8000_4003)
        );
    }

    #[test]
    fn version_three_shares_the_five_dot_oh_table() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let destination = STACK_BASE + 0xb400;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInputCreateA",
                &[0x0040_0000, 0x0300, destination, 0]
            )
            .unwrap(),
            Return(0)
        );
        assert_ne!(memory.read_u32(destination).unwrap(), 0);
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

    #[test]
    fn the_eight_dot_zero_create_answers_where_the_seven_dot_one_did() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // `DirectInput8Create(hinst, version, riidltf, ppvOut, punkOuter)`: the
        // out parameter is the fourth argument, and the version is 0x0800.
        let destination = STACK_BASE + 0xb500;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInput8Create",
                &[0x0040_0000, 0x0800, 0, destination, 0]
            )
            .unwrap(),
            Return(0)
        );
        let object = memory.read_u32(destination).unwrap();
        assert!(object > 0);
        let table = memory.read_u32(object).unwrap();
        // The 8.0 factory keeps 7.0's first five slots, so `CreateDevice` is
        // where the 7.0 table has it.
        let create_device = memory.read_u32(table + 12).unwrap();
        assert_eq!(thunks::at(create_device), Some((thunks::DINPUT, 3)));
        // Aggregation is refused the same way, at the same argument.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "DirectInput8Create",
                &[0x0040_0000, 0x0800, 0, STACK_BASE + 0xb600, 0x1234]
            )
            .unwrap(),
            Return(E_NOINTERFACE)
        );
    }
}
