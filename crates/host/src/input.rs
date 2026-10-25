//! The input the browser has and the guest has never had: a keyboard, a pointer,
//! and the Win32 message queue they feed.
//!
//! Two views of the same state, because the guest asks two different questions:
//!
//! - **DirectInput** polls. `IDirectInputDevice::GetDeviceState` returns the 256
//!   byte DIK array (or a `DIMOUSESTATE`), so the game's own frame loop reads the
//!   keyboard once per frame without any message ever being delivered.
//! - **Win32** is told. `WM_KEYDOWN` / `WM_KEYUP` / `WM_MOUSEMOVE` and the button
//!   messages go into a queue that [`crate::shim::user32::PeekMessageA`] hands
//!   out, which is what dialogs and menus use.
//!
//! Both are fed from here so they can never disagree. Scancodes are PC set 1
//! codes, which is also what DIK_* names: the page's `KeyboardEvent.code` maps to
//! the same numbers, so a key press needs no translation on the way in, only on
//! the way out ([`virtual_key`] for `wParam`, [`character`] for `WM_CHAR`).
//!
//! The queue is bounded. A guest that never pumps (the game polls instead) would
//! otherwise grow it for as long as the page is open; dropping the oldest message
//! keeps the newest state, which is the one a dialog acts on.

use std::collections::VecDeque;

use crate::shim::{DeviceKind, Host};

/// How many messages may wait for the guest. A dialog only ever looks at the
/// newest handful; anything older than this is history.
const QUEUE_LIMIT: usize = 64;

/// How many buffered DirectInput events may wait. A game that polls once a frame
/// consumes them immediately; the cap is for one that does not poll at all.
const BUFFER_LIMIT: usize = 256;

/// `DIMOFS_X`, `DIMOFS_Y`, `DIMOFS_BUTTON0`: the data-format offsets a buffered
/// mouse event names, which are byte offsets into `DIMOUSESTATE`.
const MOUSE_X: u32 = 0;
const MOUSE_Y: u32 = 4;
const MOUSE_BUTTON0: u32 = 12;

/// Mouse buttons, in the order [`Input::button`] indexes them.
pub const LEFT: u8 = 0;
pub const RIGHT: u8 = 1;
pub const MIDDLE: u8 = 2;

pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_CHAR: u32 = 0x0102;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_MBUTTONDOWN: u32 = 0x0207;
pub const WM_MBUTTONUP: u32 = 0x0208;
pub const WM_QUIT: u32 = 0x0012;

/// `MK_LBUTTON` and friends, the key-state bits `wParam` carries.
const MK_BUTTONS: [u32; 3] = [0x0001, 0x0002, 0x0010];

/// One buffered DirectInput event: what `GetDeviceData` hands out, in the order
/// it happened. A game built on buffered input reads keys here rather than from
/// `GetDeviceState`, and this is the only place the two views differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceEvent {
    pub kind: DeviceKind,
    /// Offset inside the device's data format: the scancode for a keyboard, the
    /// `DIMOFS_*` offset for a mouse.
    pub ofs: u32,
    pub data: u32,
    pub time: u32,
    pub sequence: u32,
}

/// One queued message, in the layout `MSG` has on the guest's stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub hwnd: u32,
    pub message: u32,
    pub wparam: u32,
    pub lparam: u32,
    pub time: u32,
    /// Client coordinates the message happened at, as `MSG.pt`.
    pub x: i32,
    pub y: i32,
}

impl Message {
    /// `MSG` is 28 bytes on 32-bit Windows.
    pub const BYTES: u32 = 28;

    /// The bytes a shim copies into the guest's `MSG`.
    pub fn to_bytes(self) -> [u8; Self::BYTES as usize] {
        let mut bytes = [0_u8; Self::BYTES as usize];
        for (index, value) in [self.hwnd, self.message, self.wparam, self.lparam, self.time]
            .iter()
            .enumerate()
        {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes[20..24].copy_from_slice(&self.x.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.y.to_le_bytes());
        bytes
    }
}

/// Keyboard, pointer and the queue they feed.
pub struct Input {
    /// The DIK array DirectInput reports: `0x80` down, `0` up.
    keys: [u8; 256],
    /// `GetAsyncKeyState`, indexed by virtual-key code: bit 15 is "down now" and
    /// bit 0 is "pressed since the last read", which the guest consumes.
    async_down: [bool; 256],
    async_pressed: [bool; 256],
    /// Pointer position in client coordinates.
    pub mouse: (i32, i32),
    /// Movement since the last `DIMOUSESTATE` read, which is what a DirectInput
    /// mouse reports instead of a position.
    delta: (i32, i32),
    /// Bit `n` is button `n` held.
    buttons: u8,
    messages: VecDeque<Message>,
    /// The buffered view: `GetDeviceData` empties this.
    device_events: VecDeque<DeviceEvent>,
    sequence: u32,
    /// The guest's own millisecond clock has no source in this host (the image
    /// imports no tick function), so messages are stamped from a counter. A
    /// dialog only checks ordering, never the value.
    clock: u32,
    /// How many events have been delivered, for the page and the trace.
    pub events: u64,
}

impl Default for Input {
    fn default() -> Self {
        Self::new()
    }
}

impl Input {
    pub fn new() -> Self {
        Self {
            keys: [0; 256],
            async_down: [false; 256],
            async_pressed: [false; 256],
            mouse: (0, 0),
            delta: (0, 0),
            buttons: 0,
            messages: VecDeque::new(),
            device_events: VecDeque::new(),
            sequence: 0,
            clock: 0,
            events: 0,
        }
    }

    /// The DIK array, which is what `GetDeviceState` hands back for a keyboard.
    pub fn key_state(&self) -> &[u8; 256] {
        &self.keys
    }

    pub fn held(&self, dik: u8) -> bool {
        self.keys[usize::from(dik)] != 0
    }

    pub fn shift(&self) -> bool {
        self.held(0x2a) || self.held(0x36)
    }

    fn stamp(&mut self) -> u32 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn push(&mut self, message: Message) {
        if self.messages.len() >= QUEUE_LIMIT {
            self.messages.pop_front();
        }
        self.messages.push_back(message);
        self.events += 1;
    }

    /// Records one event for the buffered DirectInput view.
    fn buffer(&mut self, kind: DeviceKind, ofs: u32, data: u32) {
        if self.device_events.len() >= BUFFER_LIMIT {
            self.device_events.pop_front();
        }
        self.sequence = self.sequence.wrapping_add(1);
        let time = self.stamp();
        self.device_events.push_back(DeviceEvent {
            kind,
            ofs,
            data,
            time,
            sequence: self.sequence,
        });
    }

    /// Takes up to `max` buffered events of one device, oldest first.
    pub fn take_device_events(&mut self, kind: DeviceKind, max: usize) -> Vec<DeviceEvent> {
        let mut taken = Vec::new();
        let mut index = 0;
        while taken.len() < max && index < self.device_events.len() {
            if self.device_events[index].kind == kind {
                taken.push(self.device_events.remove(index).expect("index is in range"));
            } else {
                index += 1;
            }
        }
        taken
    }

    pub fn device_events(&self) -> usize {
        self.device_events.len()
    }

    /// A key changed state. A repeat (`down` while already down) delivers another
    /// `WM_KEYDOWN`, as autorepeat does.
    pub fn key(&mut self, dik: u8, down: bool, hwnd: u32) {
        if dik == 0 {
            return;
        }
        let state = if down { 0x80 } else { 0 };
        self.keys[usize::from(dik)] = state;
        self.buffer(DeviceKind::Keyboard, u32::from(dik), u32::from(state));
        let vk = virtual_key(dik);
        if vk != 0 {
            let index = usize::from(vk);
            self.async_down[index] = down;
            if down {
                self.async_pressed[index] = true;
            }
        }
        // `lParam` bit 30 is the previous state and bit 31 the transition; a
        // release sets bit 31 even when the key was never seen down.
        let lparam = 1 | (u32::from(dik) << 16) | if down { 0 } else { 0xc000_0000 };
        let (x, y) = self.mouse;
        let time = self.stamp();
        self.push(Message {
            hwnd,
            message: if down { WM_KEYDOWN } else { WM_KEYUP },
            wparam: u32::from(vk),
            lparam,
            time,
            x,
            y,
        });
    }

    /// The pointer moved to a client position.
    pub fn pointer(&mut self, x: i32, y: i32, hwnd: u32) {
        if (x, y) == self.mouse {
            return;
        }
        let step_x = x - self.mouse.0;
        let step_y = y - self.mouse.1;
        self.delta.0 = self.delta.0.saturating_add(step_x);
        self.delta.1 = self.delta.1.saturating_add(step_y);
        self.mouse = (x, y);
        // A mouse's buffered data is movement, not a position.
        if step_x != 0 {
            self.buffer(DeviceKind::Mouse, MOUSE_X, step_x as u32);
        }
        if step_y != 0 {
            self.buffer(DeviceKind::Mouse, MOUSE_Y, step_y as u32);
        }
        let time = self.stamp();
        self.push(Message {
            hwnd,
            message: WM_MOUSEMOVE,
            wparam: u32::from(self.buttons),
            lparam: pack_point(x, y),
            time,
            x,
            y,
        });
    }

    /// Pointer movement with no position of its own, which is what a locked
    /// pointer reports: the deltas *are* the input, and the position is only
    /// what a message carries, so the caller keeps it in range. Unlike
    /// [`Input::pointer`], movement past the edge is not lost here -- which is
    /// the whole point, because a locked pointer has no edge to stop at.
    pub fn move_by(&mut self, dx: i32, dy: i32, x: i32, y: i32, hwnd: u32) {
        if dx == 0 && dy == 0 {
            return;
        }
        self.delta.0 = self.delta.0.saturating_add(dx);
        self.delta.1 = self.delta.1.saturating_add(dy);
        self.mouse = (x, y);
        if dx != 0 {
            self.buffer(DeviceKind::Mouse, MOUSE_X, dx as u32);
        }
        if dy != 0 {
            self.buffer(DeviceKind::Mouse, MOUSE_Y, dy as u32);
        }
        let time = self.stamp();
        self.push(Message {
            hwnd,
            message: WM_MOUSEMOVE,
            wparam: u32::from(self.buttons),
            lparam: pack_point(x, y),
            time,
            x,
            y,
        });
    }

    /// A pointer button changed state. The direct-input mouse reports buttons on
    /// its own; this only maintains the Win32 side.
    pub fn button(&mut self, button: u8, down: bool, hwnd: u32) {
        if button > MIDDLE {
            return;
        }
        let mask = 1_u8 << button;
        if down {
            self.buttons |= mask;
        } else {
            self.buttons &= !mask;
        }
        self.buffer(
            DeviceKind::Mouse,
            MOUSE_BUTTON0 + u32::from(button),
            if down { 0x80 } else { 0 },
        );
        let message = match (button, down) {
            (LEFT, true) => WM_LBUTTONDOWN,
            (LEFT, false) => WM_LBUTTONUP,
            (RIGHT, true) => WM_RBUTTONDOWN,
            (RIGHT, false) => WM_RBUTTONUP,
            (_, true) => WM_MBUTTONDOWN,
            (_, false) => WM_MBUTTONUP,
        };
        let (x, y) = self.mouse;
        let time = self.stamp();
        self.push(Message {
            hwnd,
            message,
            wparam: u32::from(MK_BUTTONS[usize::from(button)]) | u32::from(self.buttons),
            lparam: pack_point(x, y),
            time,
            x,
            y,
        });
    }

    /// `DIMOUSESTATE` movement since the last read: `lX`, `lY`, `lZ` and four
    /// button bytes.
    pub fn take_mouse_motion(&mut self) -> (i32, i32) {
        let motion = self.delta;
        self.delta = (0, 0);
        motion
    }

    pub fn button_state(&self) -> u8 {
        self.buttons
    }

    /// `GetAsyncKeyState`'s answer for a virtual-key code.
    pub fn async_state(&mut self, vk: u8) -> u16 {
        let index = usize::from(vk);
        let down = self.async_down[index];
        let pressed = std::mem::take(&mut self.async_pressed[index]);
        (if down { 0x8000 } else { 0 }) | u16::from(pressed)
    }

    /// Fills the queue with `WM_CHAR` for a key-down message, which is what
    /// `TranslateMessage` does.
    pub fn translate(&mut self, hwnd: u32, message: u32, wparam: u32) -> bool {
        if message != WM_KEYDOWN {
            return false;
        }
        let Some(character) = character(wparam as u8, self.shift()) else {
            return false;
        };
        let (x, y) = self.mouse;
        let time = self.stamp();
        self.push(Message {
            hwnd,
            message: WM_CHAR,
            wparam: u32::from(character),
            lparam: 1,
            time,
            x,
            y,
        });
        true
    }

    /// Queues a message with nothing pressed, for what the host itself says: the
    /// window gained focus, or the guest asked to quit.
    pub fn post(&mut self, hwnd: u32, message: u32, wparam: u32, lparam: u32) {
        let (x, y) = self.mouse;
        let time = self.stamp();
        self.push(Message {
            hwnd,
            message,
            wparam,
            lparam,
            time,
            x,
            y,
        });
    }

    /// Index of the first queued message that matches a `PeekMessageA` filter.
    pub fn find(&self, hwnd: u32, min: u32, max: u32) -> Option<usize> {
        self.messages.iter().position(|message| {
            (hwnd == 0 || message.hwnd == hwnd)
                && (min == 0 || max == 0 || (message.message >= min && message.message <= max))
        })
    }

    pub fn get(&self, index: usize) -> Option<Message> {
        self.messages.get(index).copied()
    }

    pub fn remove(&mut self, index: usize) -> Option<Message> {
        self.messages.remove(index)
    }

    /// Takes the oldest message, if there is one.
    pub fn next(&mut self) -> Option<Message> {
        self.messages.pop_front()
    }

    /// Takes the oldest message that matches a `PeekMessageA` filter.
    pub fn take(&mut self, hwnd: u32, min: u32, max: u32) -> Option<Message> {
        let index = self.find(hwnd, min, max)?;
        self.remove(index)
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }
}

/// Packs client coordinates the way `lParam` carries them.
pub fn pack_point(x: i32, y: i32) -> u32 {
    (u32::from(x as i16 as u16)) | (u32::from(y as i16 as u16) << 16)
}

/// The virtual-key code for a set 1 scancode, or 0 for one this host does not
/// name. The table is the US layout the guest's own dialogs assume; anything
/// past it still reaches DirectInput, which is what the game's frame loop reads.
pub fn virtual_key(dik: u8) -> u8 {
    match dik {
        0x01 => 0x1b, // escape
        0x02..=0x0a => b'1' + (dik - 0x02),
        0x0b => b'0',
        0x0c => 0xbd, // OEM_MINUS
        0x0d => 0xbb, // OEM_PLUS
        0x0e => 0x08, // backspace
        0x0f => 0x09, // tab
        0x10 => b'Q',
        0x11 => b'W',
        0x12 => b'E',
        0x13 => b'R',
        0x14 => b'T',
        0x15 => b'Y',
        0x16 => b'U',
        0x17 => b'I',
        0x18 => b'O',
        0x19 => b'P',
        0x1a => 0xdb, // OEM_4
        0x1b => 0xdd, // OEM_6
        0x1c => 0x0d, // enter
        0x1d => 0x11, // control
        0x1e => b'A',
        0x1f => b'S',
        0x20 => b'D',
        0x21 => b'F',
        0x22 => b'G',
        0x23 => b'H',
        0x24 => b'J',
        0x25 => b'K',
        0x26 => b'L',
        0x27 => 0xba, // OEM_1
        0x28 => 0xde, // OEM_7
        0x29 => 0xc0, // OEM_3
        0x2a => 0x10, // shift
        0x2b => 0xdc, // OEM_5
        0x2c => b'Z',
        0x2d => b'X',
        0x2e => b'C',
        0x2f => b'V',
        0x30 => b'B',
        0x31 => b'N',
        0x32 => b'M',
        0x33 => 0xbc, // OEM_COMMA
        0x34 => 0xbe, // OEM_PERIOD
        0x35 => 0xbf, // OEM_2
        0x36 => 0x10, // right shift
        0x37 => 0x6a, // multiply
        0x38 => 0x12, // menu (alt)
        0x39 => 0x20, // space
        0x3a => 0x14, // capital
        0x3b..=0x44 => 0x70 + (dik - 0x3b), // F1..F10
        0x45 => 0x90, // num lock
        0x46 => 0x91, // scroll
        0x47 => 0x24, // home
        0x48 => 0x26, // up
        0x49 => 0x21, // prior
        0x4a => 0x6d, // subtract
        0x4b => 0x25, // left
        0x4c => 0x65, // numpad 5
        0x4d => 0x27, // right
        0x4e => 0x6b, // add
        0x4f => 0x23, // end
        0x50 => 0x28, // down
        0x51 => 0x22, // next
        0x52 => 0x2d, // insert
        0x53 => 0x2e, // delete
        0x57 => 0x7a, // F11
        0x58 => 0x7b, // F12
        _ => 0,
    }
}

/// The character `WM_CHAR` carries for a virtual-key code, or `None` when the
/// key is not text (arrows, modifiers, function keys).
pub fn character(vk: u8, shifted: bool) -> Option<u8> {
    let base = match vk {
        b'A'..=b'Z' => {
            let letter = vk - b'A' + b'a';
            return Some(if shifted { letter.to_ascii_uppercase() } else { letter });
        }
        0x30..=0x39 => {
            if !shifted {
                return Some(vk);
            }
            match vk {
                b'1' => b'!',
                b'2' => b'@',
                b'3' => b'#',
                b'4' => b'$',
                b'5' => b'%',
                b'6' => b'^',
                b'7' => b'&',
                b'8' => b'*',
                b'9' => b'(',
                _ => b')',
            }
        }
        0x20 => b' ',
        0x0d => b'\r',
        0x08 => 0x08,
        0x09 => b'\t',
        0x1b => 0x1b,
        _ => {
            let normal = match vk {
                0xbd => b'-',
                0xbb => b'=',
                0xdb => b'[',
                0xdd => b']',
                0xba => b';',
                0xde => b'\'',
                0xc0 => b'`',
                0xdc => b'\\',
                0xbc => b',',
                0xbe => b'.',
                0xbf => b'/',
                _ => return None,
            };
            if !shifted {
                return Some(normal);
            }
            match normal {
                b'-' => b'_',
                b'=' => b'+',
                b'[' => b'{',
                b']' => b'}',
                b';' => b':',
                b'\'' => b'"',
                b'`' => b'~',
                b'\\' => b'|',
                b',' => b'<',
                b'.' => b'>',
                b'/' => b'?',
                other => other,
            }
        }
    };
    Some(base)
}

/// The window input goes to: the guest's declared window, since this host has
/// one window manager and it is the page.
pub fn focus(host: &Host) -> u32 {
    host.windows.main_window()
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: u32 = 0x10100;

    #[test]
    fn movement_at_the_edge_is_still_delivered() {
        let mut input = Input::new();
        // The cursor already sits in the bottom-right corner of the display.
        input.pointer(639, 479, WINDOW);
        input.take_mouse_motion();
        let _ = input.take_device_events(DeviceKind::Mouse, 64);

        // A locked pointer keeps reporting movement there. Deriving the delta
        // from a clamped position would make this zero, which is exactly the
        // case where the game stopped responding to the mouse.
        input.move_by(0, 12, 639, 479, WINDOW);
        assert_eq!(input.take_mouse_motion(), (0, 12));
        let events = input.take_device_events(DeviceKind::Mouse, 64);
        assert!(
            events
                .iter()
                .any(|event| event.ofs == MOUSE_Y && event.data == 12),
            "the buffered view reports the move too: {events:?}"
        );
    }

    #[test]
    fn a_press_sets_the_direct_input_byte_and_queues_a_keydown() {
        let mut input = Input::new();
        input.key(0x1e, true, WINDOW); // 'A'
        assert!(input.held(0x1e));
        assert_eq!(input.key_state()[0x1e], 0x80);

        let message = input.next().expect("a message");
        assert_eq!(message.message, WM_KEYDOWN);
        assert_eq!(message.wparam, u32::from(b'A'));
        assert_eq!(message.lparam & 0xffff, 1, "repeat count");
        assert_eq!((message.lparam >> 16) & 0xff, 0x1e, "scancode");
        assert!(input.is_empty());

        input.key(0x1e, false, WINDOW);
        assert!(!input.held(0x1e));
        let release = input.next().expect("a release");
        assert_eq!(release.message, WM_KEYUP);
        assert_eq!(release.lparam & 0xc000_0000, 0xc000_0000, "previous and current state");
    }

    #[test]
    fn set_1_scancodes_map_to_virtual_keys() {
        assert_eq!(virtual_key(0x1e), b'A');
        assert_eq!(virtual_key(0x39), 0x20);
        assert_eq!(virtual_key(0x1c), 0x0d);
        assert_eq!(virtual_key(0x48), 0x26, "up arrow");
        assert_eq!(virtual_key(0x3b), 0x70, "F1");
        assert_eq!(virtual_key(0x02), b'1');
        assert_eq!(virtual_key(0x0b), b'0');
        assert_eq!(virtual_key(0xff), 0, "an unnamed scan is not a key");
    }

    #[test]
    fn characters_follow_the_shift_key() {
        assert_eq!(character(b'A', false), Some(b'a'));
        assert_eq!(character(b'A', true), Some(b'A'));
        assert_eq!(character(b'1', false), Some(b'1'));
        assert_eq!(character(b'1', true), Some(b'!'));
        assert_eq!(character(0x20, false), Some(b' '));
        assert_eq!(character(0x26, false), None, "an arrow is not text");
        assert_eq!(character(0xbd, true), Some(b'_'));
    }

    #[test]
    fn translate_turns_a_keydown_into_a_char() {
        let mut input = Input::new();
        assert!(input.translate(WINDOW, WM_KEYDOWN, u32::from(b'H')));
        let message = input.next().expect("a character");
        assert_eq!(message.message, WM_CHAR);
        assert_eq!(message.wparam, u32::from(b'h'));
        // A key that is not text produces nothing.
        assert!(!input.translate(WINDOW, WM_KEYDOWN, 0x26));
        assert!(!input.translate(WINDOW, WM_KEYUP, u32::from(b'H')));
    }

    #[test]
    fn async_state_reports_down_and_pressed_once() {
        let mut input = Input::new();
        input.key(0x1e, true, WINDOW);
        assert_eq!(input.async_state(b'A'), 0x8001);
        assert_eq!(input.async_state(b'A'), 0x8000, "the press edge is consumed");
        input.key(0x1e, false, WINDOW);
        assert_eq!(input.async_state(b'A'), 0);
        // A release injects nothing.
        assert_eq!(input.async_state(b'B'), 0);
    }

    #[test]
    fn the_pointer_reports_a_position_and_a_delta() {
        let mut input = Input::new();
        // The first move carries the distance from the origin, which is where a
        // DirectInput mouse starts.
        input.pointer(100, 50, WINDOW);
        assert_eq!(input.take_mouse_motion(), (100, 50));
        input.pointer(110, 40, WINDOW);
        assert_eq!(input.mouse, (110, 40));
        assert_eq!(input.take_mouse_motion(), (10, -10));
        assert_eq!(input.take_mouse_motion(), (0, 0), "movement is consumed");

        let message = input.next().expect("a move");
        assert_eq!(message.message, WM_MOUSEMOVE);
        assert_eq!(message.x, 100);
        assert_eq!(message.y, 50);
        assert_eq!(message.lparam, pack_point(100, 50));
    }

    #[test]
    fn a_button_press_carries_the_button_bits() {
        let mut input = Input::new();
        input.pointer(320, 240, WINDOW);
        input.button(LEFT, true, WINDOW);
        assert_eq!(input.button_state(), 1);
        let press = { input.next(); input.next().expect("a press") };
        assert_eq!(press.message, WM_LBUTTONDOWN);
        assert_eq!(press.wparam & 1, 1, "MK_LBUTTON");
        assert_eq!(press.lparam, pack_point(320, 240));

        input.button(LEFT, false, WINDOW);
        assert_eq!(input.button_state(), 0);
        assert_eq!(input.next().unwrap().message, WM_LBUTTONUP);
    }

    #[test]
    fn the_queue_is_bounded_and_keeps_the_newest() {
        let mut input = Input::new();
        for index in 0..(QUEUE_LIMIT as u32 + 8) {
            input.post(WINDOW, WM_CHAR, index, 0);
        }
        assert_eq!(input.len(), QUEUE_LIMIT);
        assert_eq!(input.get(0).unwrap().wparam, 8);
        assert_eq!(input.next().unwrap().wparam, 8);
    }

    #[test]
    fn a_peek_filter_matches_window_and_range() {
        let mut input = Input::new();
        input.post(WINDOW, WM_KEYDOWN, 1, 0);
        input.post(WINDOW + 4, WM_MOUSEMOVE, 0, 0);
        input.post(WINDOW, WM_CHAR, 2, 0);

        assert_eq!(input.find(0, 0, 0), Some(0));
        assert_eq!(input.find(WINDOW + 4, 0, 0), Some(1));
        assert_eq!(input.find(WINDOW, WM_CHAR, WM_CHAR), Some(2));
        assert_eq!(input.find(WINDOW, 0x0300, 0x0400), None);
        assert_eq!(input.remove(2).unwrap().wparam, 2);
        assert_eq!(input.len(), 2);
    }

    #[test]
    fn a_message_is_twenty_eight_bytes_of_the_guest_layout() {
        let message = Message {
            hwnd: 0x10100,
            message: WM_MOUSEMOVE,
            wparam: 3,
            lparam: pack_point(-4, 5),
            time: 9,
            x: -4,
            y: 5,
        };
        let bytes = message.to_bytes();
        assert_eq!(bytes.len() as u32, Message::BYTES);
        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 0x10100);
        assert_eq!(u32::from_le_bytes(bytes[4..8].try_into().unwrap()), WM_MOUSEMOVE);
        assert_eq!(u32::from_le_bytes(bytes[16..20].try_into().unwrap()), 9);
        assert_eq!(i32::from_le_bytes(bytes[20..24].try_into().unwrap()), -4);
        assert_eq!(i32::from_le_bytes(bytes[24..28].try_into().unwrap()), 5);
    }
}
