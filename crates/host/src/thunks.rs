//! Guest-callable host objects: the thunk range, and the interfaces behind it.
//!
//! A Win32 import is a slot in the IAT, so the step loop recognises it by
//! address. A COM method is different: the guest loads a vtable pointer from an
//! object the *host* made and calls `[vtable + offset]`, so the call target is a
//! number the host chose. This module gives those numbers a shape:
//!
//! ```text
//! address = THUNK_BASE + (kind << 8 | slot) * 4
//! ```
//!
//! `kind` names the interface and `slot` is the vtable entry (`offset / 4`), so a
//! thunk's address says exactly which method was called. [`serve`] answers it, and
//! the step loop's argument handling is the same as for an import: the caller
//! pushed its return address, the arguments sit above it, and the shim pops them
//! per its own declared signature. `this` is argument 0, as it is on Windows.
//!
//! # Refusing is part of the design
//!
//! A slot this module does not model is an error naming the interface, the slot
//! and the vtable offset -- not a zero return. A wrong pop count would corrupt the
//! guest's stack and a wrong success would send it down a path with an object that
//! cannot support it. Stopping at the first method that is not answered says
//! exactly which one the guest wanted, which is how this table grows.
//!
//! `DirectInput` is here because the guest creates its input object *before*
//! DirectDraw and returns failure if it cannot; the object it gets reports no keys
//! held, which is the truth until input exists. `DirectDraw` arrives next, with
//! kinds [`DDRAW`] and [`DDRAW_SURFACE`] reserved for it.

use crate::memory::GuestMemory;
use crate::shim::{Action, Call, DeviceKind, Host};
use crate::Result;

use Action::Return;

/// Where thunks live. Below `HOST_FUNCTION_BASE`, and above nothing else the host
/// maps, so a program counter there is unambiguous.
pub const THUNK_BASE: u32 = 0x00e0_0000;

/// Sixteen interface kinds of up to 64 methods each is 4 KB of address space.
pub const KIND_LIMIT: u8 = 16;
pub const SLOT_LIMIT: u8 = 64;

/// Interface kinds.
pub const DINPUT: u8 = 0;
pub const DINPUT_DEVICE: u8 = 1;
pub const DDRAW: u8 = 2;
pub const DDRAW_SURFACE: u8 = 3;
pub const DDRAW_PALETTE: u8 = 4;
pub const DDRAW_CLIPPER: u8 = 5;
pub const DSOUND: u8 = 6;
pub const DSOUND_BUFFER: u8 = 7;

const S_OK: u32 = 0;
/// The object exists but does not implement the requested interface.
const E_NOINTERFACE: u32 = 0x8000_4002;

/// `GUID_SysKeyboard` and `GUID_SysMouse` share everything but their first
/// dword, which is what tells a device apart: the guest passes the whole GUID to
/// `CreateDevice`, and the rest of it is `D5A0-11CF-BFC7-444553540000` for both.
const GUID_SYS_MOUSE: u32 = 0x6F1D_2B60;
const GUID_SYS_KEYBOARD: u32 = 0x6F1D_2B61;
const GUID_SYS_JOYSTICK: u32 = 0x6F1D_2B70;

/// `DIDATAFORMAT.dwDataSize`, which says what the device's state looks like: 256
/// bytes of DIK codes, a 20-byte `DIMOUSESTATE`, or a joystick struct.
const DIDATAFORMAT_DATA_SIZE: u32 = 12;
/// `DIMOUSESTATE`: three longs and four button bytes, which is 16 bytes. The
/// guest asks for exactly 16, so this is the whole structure rather than a
/// prefix of it.
const DIMOUSESTATE_BYTES: usize = 16;

/// A device's kind from the GUID it was created with. An unknown GUID is treated
/// as a keyboard: the game's input is polled from the keyboard, so that is the
/// reading that keeps it working, and `SetDataFormat` can correct it.
fn device_kind(memory: &GuestMemory, guid: u32) -> DeviceKind {
    match memory.read_u32(guid) {
        Ok(GUID_SYS_MOUSE) => DeviceKind::Mouse,
        Ok(GUID_SYS_JOYSTICK) => DeviceKind::Joystick,
        Ok(GUID_SYS_KEYBOARD) => DeviceKind::Keyboard,
        _ => DeviceKind::Keyboard,
    }
}

/// One vtable method: its name and the bytes the callee pops, `this` included.
#[derive(Debug)]
pub(crate) struct Method {
    pub(crate) name: &'static str,
    pub(crate) pop: u32,
}

/// Shorthand for a method table entry.
#[allow(dead_code)]
pub(crate) const fn m(name: &'static str, pop: u32) -> Option<Method> {
    Some(Method { name, pop })
}

/// `IDirectInput`, in vtable order. Any gap is an entry this table does not model.
const DINPUT_METHODS: &[Option<Method>] = &[
    Some(Method { name: "QueryInterface", pop: 12 }),
    Some(Method { name: "AddRef", pop: 4 }),
    Some(Method { name: "Release", pop: 4 }),
    Some(Method { name: "CreateDevice", pop: 16 }),
];

/// `IDirectInputDevice`, in vtable order. The offsets match the plan's table:
/// `SetProperty` 0x18, `Acquire` 0x1C, `GetDeviceState` 0x24, `SetDataFormat` 0x2C,
/// `SetCooperativeLevel` 0x34.
const DINPUT_DEVICE_METHODS: &[Option<Method>] = &[
    Some(Method { name: "QueryInterface", pop: 12 }),
    Some(Method { name: "AddRef", pop: 4 }),
    Some(Method { name: "Release", pop: 4 }),
    Some(Method { name: "GetCapabilities", pop: 8 }),
    Some(Method { name: "EnumObjects", pop: 16 }),
    Some(Method { name: "GetProperty", pop: 12 }),
    Some(Method { name: "SetProperty", pop: 12 }),
    Some(Method { name: "Acquire", pop: 4 }),
    Some(Method { name: "Unacquire", pop: 4 }),
    Some(Method { name: "GetDeviceState", pop: 12 }),
    Some(Method { name: "GetDeviceData", pop: 20 }),
    Some(Method { name: "SetDataFormat", pop: 8 }),
    Some(Method { name: "SetEventNotification", pop: 8 }),
    Some(Method { name: "SetCooperativeLevel", pop: 12 }),
    Some(Method { name: "GetObjectInfo", pop: 16 }),
    Some(Method { name: "GetDeviceInfo", pop: 8 }),
    Some(Method { name: "RunControlPanel", pop: 12 }),
    Some(Method { name: "Initialize", pop: 16 }),
];

/// The thunk address for a method.
pub fn address(kind: u8, slot: u8) -> u32 {
    THUNK_BASE + ((u32::from(kind) << 8) | u32::from(slot)) * 4
}

/// Which method a program counter names, if it is in the thunk range at all.
pub fn at(eip: u32) -> Option<(u8, u8)> {
    let offset = eip.wrapping_sub(THUNK_BASE);
    // The encoding reserves a whole byte for the slot, so the range is one
    // `kind` byte wide, not `KIND_LIMIT * SLOT_LIMIT` entries: a bound written
    // from the table sizes would reject every kind from 4 upwards.
    if offset >= (u32::from(KIND_LIMIT) << 8) * 4 {
        return None;
    }
    if offset & 3 != 0 {
        return None;
    }
    let id = offset / 4;
    Some(((id >> 8) as u8, (id & 0xff) as u8))
}

/// The interface's name, for diagnostics.
pub fn kind_name(kind: u8) -> &'static str {
    match kind {
        DINPUT => "IDirectInput",
        DINPUT_DEVICE => "IDirectInputDevice",
        DDRAW => "IDirectDraw",
        DDRAW_SURFACE => "IDirectDrawSurface",
        DDRAW_PALETTE => "IDirectDrawPalette",
        DDRAW_CLIPPER => "IDirectDrawClipper",
        DSOUND => "IDirectSound",
        DSOUND_BUFFER => "IDirectSoundBuffer",
        _ => "unknown interface",
    }
}

fn methods(kind: u8) -> Option<&'static [Option<Method>]> {
    match kind {
        DINPUT => Some(DINPUT_METHODS),
        DINPUT_DEVICE => Some(DINPUT_DEVICE_METHODS),
        DDRAW => Some(crate::shim::ddraw::DRAW_METHODS),
        DDRAW_SURFACE => Some(crate::shim::ddraw::SURFACE_METHODS),
        DDRAW_PALETTE => Some(crate::shim::ddraw::PALETTE_METHODS),
        DDRAW_CLIPPER => Some(crate::shim::ddraw::CLIPPER_METHODS),
        DSOUND => Some(crate::shim::dsound::SOUND_METHODS),
        DSOUND_BUFFER => Some(crate::shim::dsound::BUFFER_METHODS),
        _ => None,
    }
}

/// The method at `slot`, or an error naming what is missing.
fn method(kind: u8, slot: u8) -> Result<&'static Method> {
    let entries = methods(kind)
        .ok_or_else(|| format!("{} is not modelled yet", kind_name(kind)))?;
    entries
        .get(usize::from(slot))
        .and_then(Option::as_ref)
        .ok_or_else(|| {
            format!(
                "{}.slot {slot} (offset {:#04x}) is not modelled",
                kind_name(kind),
                u32::from(slot) * 4
            )
        })
}

/// Argument bytes the method pops, `this` included. The step loop needs this
/// before the shim runs, exactly as it does for an import.
pub fn arg_bytes(kind: u8, slot: u8) -> u32 {
    method(kind, slot).map_or(4, |method| method.pop)
}

/// `Interface.Method` for a call, for logs.
pub fn label(kind: u8, slot: u8) -> String {
    match method(kind, slot) {
        Ok(method) => format!("{}.{}", kind_name(kind), method.name),
        Err(_) => format!("{}.slot {slot}", kind_name(kind)),
    }
}

/// Answers one method call.
pub fn serve(host: &mut Host, call: &mut Call<'_>, kind: u8, slot: u8) -> Result<Action> {
    let method = method(kind, slot)?;
    match (kind, method.name) {
        // DirectSound buffers are reference counted for real: the last Release
        // frees the guest's sample memory. `dsound::serve` owns every method of
        // its two interfaces, including the reference count.
        (DSOUND | DSOUND_BUFFER, name) => {
            crate::shim::dsound::serve(host, call, kind, name)
        }

        // Reference counting: on a host object that outlives its references, the
        // count only has to be non-zero and stable. Surfaces are the exception --
        // `ddraw::serve` owns their Release, because the last one frees pixels.
        (_, "AddRef") | (_, "Release") => Ok(Return(1)),
        (_, "QueryInterface") => Ok(Return(E_NOINTERFACE)),

        (DINPUT, "CreateDevice") => {
            let kind = device_kind(call.memory, call.arg(1)?);
            let destination = call.arg(2)?;
            let device = new_object(call.memory, host, DINPUT_DEVICE)?;
            host.devices.insert(device, kind);
            host.notice(format!(
                "DirectInput: {} created; its state comes from the page's {}",
                match kind {
                    DeviceKind::Keyboard => "a keyboard device",
                    DeviceKind::Mouse => "a mouse device",
                    DeviceKind::Joystick => "a joystick device",
                },
                match kind {
                    DeviceKind::Keyboard => "keyboard",
                    DeviceKind::Mouse => "pointer",
                    DeviceKind::Joystick => "(nothing: no joystick is attached)",
                }
            ));
            call.write_u32(destination, device)?;
            Ok(Return(S_OK))
        }

        (DINPUT_DEVICE, "GetCapabilities") => {
            let caps = call.arg(1)?;
            if caps != 0 {
                // DIDEVICEOBJECTINSTANCE-free caps: a 44-byte structure whose
                // fields the guest reads for device type and axis count.
                call.zero(caps, 44)?;
            }
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "EnumObjects") => {
            // No axes, buttons or sliders: the callback is never invoked, which is
            // what a device with no objects would do.
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "GetProperty") => {
            let header = call.arg(2)?;
            if header != 0 {
                // A DIPROPHEADER is 16 bytes and the value follows it.
                call.zero(header, 16)?;
            }
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "SetProperty") => Ok(Return(S_OK)),
        (DINPUT_DEVICE, "Acquire") | (DINPUT_DEVICE, "Unacquire") => Ok(Return(S_OK)),
        (DINPUT_DEVICE, "GetDeviceState") => {
            host.device_state_reads += 1;
            let device = call.arg(0)?;
            let length = call.arg(1)?;
            let destination = call.arg(2)?;
            if destination != 0 && length != 0 {
                let kind = host
                    .devices
                    .get(&device)
                    .copied()
                    .unwrap_or(DeviceKind::Keyboard);
                // Start from "nothing held" so a caller's buffer is fully written
                // whatever it asked for, then lay the real state over the part
                // that fits.
                call.zero(destination, length)?;
                match kind {
                    DeviceKind::Keyboard => {
                        let count = (length as usize).min(256);
                        let keys = &host.input.key_state()[..count];
                        call.write(destination, keys)?;
                        // Report the first keyboard read that sees a held key, once
                        // per session. When a key does nothing in the game, this is
                        // what separates "the page never sent it" from "the guest
                        // read it and decided against it".
                        if keys.iter().any(|key| *key != 0)
                            && !host
                                .notices
                                .iter()
                                .any(|notice| notice.starts_with("keyboard read"))
                        {
                            let held: Vec<String> = keys
                                .iter()
                                .enumerate()
                                .filter(|(_, key)| **key != 0)
                                .map(|(index, _)| format!("{index:#04x}"))
                                .collect();
                            host.notice(format!(
                                "keyboard read while {} held, length {length}",
                                held.join(",")
                            ));
                        }
                    }
                    DeviceKind::Mouse => {
                        let (dx, dy) = host.input.take_mouse_motion();
                        let mut state = [0_u8; DIMOUSESTATE_BYTES];
                        state[0..4].copy_from_slice(&dx.to_le_bytes());
                        state[4..8].copy_from_slice(&dy.to_le_bytes());
                        state[12..16].copy_from_slice(&button_bytes(host.input.button_state()));
                        let count = (length as usize).min(DIMOUSESTATE_BYTES);
                        call.write(destination, &state[..count])?;
                    }
                    // No joystick is attached, so every axis and button stays at
                    // the zero the buffer already holds.
                    DeviceKind::Joystick => {}
                }
            }
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "GetDeviceData") => {
            // The buffered view: `GetDeviceData(this, cbObjectData, rgdod,
            // pdwInOut, dwFlags)`. The count is in and out, and each element is
            // a `DIDEVICEOBJECTDATA`: offset, data, time, sequence, app data.
            host.device_data_reads += 1;
            let device = call.arg(0)?;
            // The guest's own element size: this build uses 16, which is the
            // `DIDEVICEOBJECTDATA` fields without the trailing application data.
            // Writing the full 20 would run into the next element.
            let stride = call.arg(1)?;
            let array = call.arg(2)?;
            let count_pointer = call.arg(3)?;
            if array == 0 || count_pointer == 0 {
                return Ok(Return(S_OK));
            }
            let kind = host
                .devices
                .get(&device)
                .copied()
                .unwrap_or(DeviceKind::Keyboard);
            let wanted = call.memory.read_u32(count_pointer)? as usize;
            let events = host.input.take_device_events(kind, wanted);
            for (index, event) in events.iter().enumerate() {
                let at = array.wrapping_add(index as u32 * stride.max(4));
                for (field, value) in [event.ofs, event.data, event.time, event.sequence, 0]
                    .iter()
                    .enumerate()
                {
                    if (field as u32 + 1) * 4 <= stride {
                        call.write_u32(at.wrapping_add(field as u32 * 4), *value)?;
                    }
                }
            }
            call.write_u32(count_pointer, events.len() as u32)?;
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "SetDataFormat") => {
            // The data format is the authoritative statement of what the device
            // returns, so it can correct a GUID this host did not recognise.
            let device = call.arg(0)?;
            // `SetDataFormat(this, lpDataFormat)`: two arguments, not three.
            let format = call.arg(1)?;
            let size = call
                .memory
                .read_u32(format.wrapping_add(DIDATAFORMAT_DATA_SIZE))
                .unwrap_or(0);
            let kind = match size {
                256 => Some(DeviceKind::Keyboard),
                16 => Some(DeviceKind::Mouse),
                _ => None,
            };
            if let Some(kind) = kind {
                host.devices.insert(device, kind);
            }
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "SetEventNotification")
        | (DINPUT_DEVICE, "SetCooperativeLevel")
        | (DINPUT_DEVICE, "RunControlPanel")
        | (DINPUT_DEVICE, "Initialize") => Ok(Return(S_OK)),
        (DINPUT_DEVICE, "GetObjectInfo") => {
            let info = call.arg(1)?;
            if info != 0 {
                call.zero(info, 24)?;
            }
            Ok(Return(S_OK))
        }
        (DINPUT_DEVICE, "GetDeviceInfo") => {
            let info = call.arg(1)?;
            if info != 0 {
                call.zero(info, 40)?;
            }
            Ok(Return(S_OK))
        }

        (DDRAW | DDRAW_SURFACE | DDRAW_PALETTE | DDRAW_CLIPPER, _) => {
            crate::shim::ddraw::serve(host, call, kind, method.name)
        }

        (kind, name) => Err(format!(
            "{}::{name} has no shim: the object exists but this method does",
            kind_name(kind)
        )),
    }
}

/// `DIMOUSESTATE.rgbButtons`: one byte per button, `0x80` when held.
fn button_bytes(buttons: u8) -> [u8; 4] {
    let mut state = [0_u8; 4];
    for (index, byte) in state.iter_mut().enumerate() {
        if buttons & (1 << index) != 0 {
            *byte = 0x80;
        }
    }
    state
}

/// Allocates a host object of `kind`: four bytes of vtable pointer, in guest
/// memory, pointing at a table of thunks.
pub fn new_object(memory: &mut GuestMemory, host: &mut Host, kind: u8) -> Result<u32> {
    let table = vtable(memory, host, kind)?;
    let object = host.allocate(4, 4)?;
    memory.write_u32(object, table)?;
    Ok(object)
}

/// The vtable for `kind`, built once per session and shared by its objects: every
/// entry points at the thunk for `(kind, slot)`, so the object only has to hold the
/// table pointer.
fn vtable(memory: &mut GuestMemory, host: &mut Host, kind: u8) -> Result<u32> {
    let index = usize::from(kind);
    let Some(cached) = host.vtables.get(index).copied() else {
        return Err(format!("interface kind {kind} is outside the vtable cache"));
    };
    if cached != 0 {
        return Ok(cached);
    }
    let slots = u32::from(SLOT_LIMIT);
    let table = host.allocate(slots * 4, 4)?;
    for slot in 0..slots {
        memory.write_u32(table + slot * 4, address(kind, slot as u8))?;
    }
    host.vtables[index] = table;
    Ok(table)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pe::IMAGE_BASE;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    #[test]
    fn a_thunk_address_names_its_interface_and_slot() {
        let address = address(DINPUT_DEVICE, 13);
        assert_eq!(at(address), Some((DINPUT_DEVICE, 13)));
        assert_eq!(label(DINPUT_DEVICE, 13), "IDirectInputDevice.SetCooperativeLevel");
        // Four bytes per entry: an address inside one is not a method.
        assert_eq!(at(address + 2), None);
        assert_eq!(at(THUNK_BASE - 4), None);
        assert_eq!(at(THUNK_BASE + 4 * 4096), None);
    }

    #[test]
    fn every_address_the_encoding_can_produce_reads_back_as_its_method() {
        for kind in 0..KIND_LIMIT {
            for slot in 0..SLOT_LIMIT {
                let thunk = address(kind, slot);
                assert_eq!(at(thunk), Some((kind, slot)), "{thunk:#010x}");
            }
        }
        // One past the last kind is outside the range.
        assert_eq!(at(address(KIND_LIMIT, 0)), None);
    }

    #[test]
    fn the_thunk_range_does_not_overlap_an_import_stub() {
        // A table can be any length; the range the stubs occupy must stay clear
        // of the thunks whatever the target imports.
        for index in 0..2048_u32 {
            let stub = crate::cpu::HOST_FUNCTION_BASE + index * 4;
            assert_eq!(at(stub), None, "stub {index} is in the thunk range");
        }
        // The thunks live between the image and the import stubs: above everything
        // the image maps, below `HOST_FUNCTION_BASE`.
        assert!(THUNK_BASE > IMAGE_BASE);
        let last = address(KIND_LIMIT - 1, SLOT_LIMIT - 1);
        assert!(last < crate::cpu::HOST_FUNCTION_BASE, "{last:#010x}");
    }

    #[test]
    fn every_modelled_method_declares_argument_bytes_including_this() {
        for (kind, entries) in [
            (DINPUT, DINPUT_METHODS),
            (DINPUT_DEVICE, DINPUT_DEVICE_METHODS),
        ] {
            for (slot, method) in entries.iter().enumerate() {
                let Some(method) = method else { continue };
                assert_eq!(method.pop % 4, 0, "{} is not dword aligned", method.name);
                assert!(method.pop >= 4, "{} does not include this", method.name);
                assert_eq!(arg_bytes(kind, slot as u8), method.pop);
            }
        }
    }

    #[test]
    fn an_unmodelled_slot_is_an_error_naming_the_offset() {
        let error = method(DINPUT_DEVICE, 40).unwrap_err();
        assert!(error.contains("IDirectInputDevice"), "{error}");
        assert!(error.contains("0xa0"), "{error}");
        // DirectDraw has a table, so the first unmodelled slot is past its end.
        let error = method(DDRAW, 60).unwrap_err();
        assert!(error.contains("IDirectDraw"), "{error}");
        assert!(error.contains("0xf0"), "{error}");
    }

    #[test]
    fn an_object_points_at_its_interfaces_thunks() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let object = new_object(&mut memory, &mut host, DINPUT).unwrap();
        let table = memory.read_u32(object).unwrap();
        // CreateDevice is slot 3, and the guest calls it through that entry.
        let entry = memory.read_u32(table + 12).unwrap();
        assert_eq!(at(entry), Some((DINPUT, 3)));
        // Two objects of a kind share one table.
        let second = new_object(&mut memory, &mut host, DINPUT).unwrap();
        assert_eq!(memory.read_u32(second).unwrap(), table);
        assert_ne!(object, second);
    }

    #[test]
    fn create_device_hands_back_a_device_object() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let args = crate::memory::STACK_BASE + 0x3000;
        let destination = crate::memory::STACK_BASE + 0x3100;
        for (index, value) in [0x0040_0000_u32, 0x0500, destination, 0].iter().enumerate() {
            memory.write_u32(args + index as u32 * 4, *value).unwrap();
        }
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 16,
        };
        assert_eq!(
            serve(&mut host, &mut call, DINPUT, 3).unwrap(),
            Return(0)
        );
        let device = call.memory.read_u32(destination).unwrap();
        assert!(device > 0);
        let table = call.memory.read_u32(device).unwrap();
        assert_eq!(at(call.memory.read_u32(table + 0x24).unwrap()), Some((DINPUT_DEVICE, 9)));
    }

    #[test]
    fn a_held_key_reaches_the_polled_state_and_the_event_buffer() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = new_object(&mut memory, &mut host, DINPUT_DEVICE).unwrap();
        host.devices.insert(device, DeviceKind::Keyboard);
        host.key(0x1e, true); // 'A'

        // The polled view: a 256-byte DIK array with 0x80 where the key is held.
        let args = crate::memory::STACK_BASE + 0x4000;
        let buffer = crate::memory::STACK_BASE + 0x4100;
        memory.write_u32(args, device).unwrap();
        memory.write_u32(args + 4, 256).unwrap();
        memory.write_u32(args + 8, buffer).unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(serve(&mut host, &mut call, DINPUT_DEVICE, 9).unwrap(), Return(0));
        assert_eq!(call.memory.read_bytes(buffer, 256).unwrap()[0x1e], 0x80);
        assert_eq!(call.memory.read_bytes(buffer, 256).unwrap()[0x1f], 0);

        // The buffered view: one 16-byte DIDEVICEOBJECTDATA for the press.
        let array = crate::memory::STACK_BASE + 0x4200;
        let count = crate::memory::STACK_BASE + 0x4300;
        call.arg_bytes = 20;
        call.memory.write_u32(args + 4, 20).unwrap();
        call.memory.write_u32(args + 8, array).unwrap();
        call.memory.write_u32(args + 12, count).unwrap();
        call.memory.write_u32(args + 16, 0).unwrap();
        call.memory.write_u32(count, 4).unwrap();
        assert_eq!(serve(&mut host, &mut call, DINPUT_DEVICE, 10).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(count).unwrap(), 1, "one event written");
        assert_eq!(call.memory.read_u32(array).unwrap(), 0x1e, "the scancode");
        assert_eq!(call.memory.read_u32(array + 4).unwrap(), 0x80, "pressed");

        // Reading again finds nothing, so the guest is not told twice.
        call.memory.write_u32(count, 4).unwrap();
        assert_eq!(serve(&mut host, &mut call, DINPUT_DEVICE, 10).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(count).unwrap(), 0);

        host.key(0x1e, false);
        call.memory.write_u32(count, 4).unwrap();
        assert_eq!(serve(&mut host, &mut call, DINPUT_DEVICE, 10).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(count).unwrap(), 1, "the release is buffered too");
        assert_eq!(call.memory.read_u32(array + 4).unwrap(), 0);
    }

    #[test]
    fn the_mouse_reports_movement_and_its_buttons() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = new_object(&mut memory, &mut host, DINPUT_DEVICE).unwrap();
        host.devices.insert(device, DeviceKind::Mouse);
        host.pointer(100, 40);
        host.pointer(110, 50);
        host.pointer_button(0, true);

        let args = crate::memory::STACK_BASE + 0x4400;
        let state = crate::memory::STACK_BASE + 0x4500;
        memory.write_u32(args, device).unwrap();
        memory.write_u32(args + 4, 16).unwrap();
        memory.write_u32(args + 8, state).unwrap();
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(serve(&mut host, &mut call, DINPUT_DEVICE, 9).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(state).unwrap() as i32, 110, "lX");
        assert_eq!(call.memory.read_u32(state + 4).unwrap() as i32, 50, "lY");
        assert_eq!(call.memory.read_u32(state + 12).unwrap(), 0x0000_0080, "button 0 down");

        // Movement is consumed by the read, as a DirectInput mouse's is.
        call.memory.write_u32(args + 8, state).unwrap();
        assert_eq!(serve(&mut host, &mut call, DINPUT_DEVICE, 9).unwrap(), Return(0));
        assert_eq!(call.memory.read_u32(state).unwrap(), 0);
        assert_eq!(call.memory.read_u32(state + 4).unwrap(), 0);
        assert_eq!(call.memory.read_u32(state + 12).unwrap(), 0x0000_0080);
    }

    #[test]
    fn no_key_is_held_and_the_buffer_is_zeroed() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let args = crate::memory::STACK_BASE + 0x3200;
        let buffer = crate::memory::STACK_BASE + 0x3300;
        memory.write_bytes(buffer, &[0xff; 256]).unwrap();
        memory.write_u32(args, 0x1234).unwrap(); // this
        memory.write_u32(args + 4, 256).unwrap(); // cbData
        memory.write_u32(args + 8, buffer).unwrap(); // lpvData
        let mut call = Call {
            memory: &mut memory,
            args,
            arg_bytes: 12,
        };
        assert_eq!(
            serve(&mut host, &mut call, DINPUT_DEVICE, 9).unwrap(),
            Return(0)
        );
        assert_eq!(call.memory.read_bytes(buffer, 256).unwrap(), &[0_u8; 256]);
    }
}
