//! The `SDL.dll` subset an SDL 1.2 game calls.
//!
//! SDL 1.2 is a thin wrapper over the same machine this host already models:
//! `SDL_SetVideoMode` hands back a `SDL_Surface` whose `pixels` an application
//! writes directly, `SDL_Flip` presents it, and the keyboard arrives as events
//! rather than as polled state. So the shim does not reimplement SDL's
//! DirectDraw backend; it gives the guest the buffer SDL would have given it and
//! publishes that buffer as the frame a page reads.
//!
//! Conventions worth naming:
//!
//! - **The exports are cdecl**, unlike every Win32 DLL. [`crate::shim::is_cdecl`]
//!   makes the session leave the arguments for the caller to pop; the declared
//!   bytes here are the shim's read window, not a pop count.
//! - **The surface is the guest's own memory.** `SDL_Flip` copies nothing and
//!   increments the host's frame counter, because the page reads the pixels the
//!   guest just wrote. A page must therefore read the framebuffer only after a
//!   flip, which is what it does anyway.
//! - **Audio is opened and not mixed yet.** `SDL_OpenAudio` reports success with
//!   the requested format so a game that treats sound as optional keeps going,
//!   and the host records one notice saying the mixer is not wired. Silence here
//!   is a missing feature, not a claim that the game is silent.
//! - **`SDL_WaitEvent` does not block.** This machine runs the guest in a frame
//!   budget; blocking would stall the page. It answers like `SDL_PollEvent`, and
//!   a guest that waits is expected to see the next frame's events instead.

use crate::input::{self, Message};
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const SDL: &str = "SDL.dll";

/// `SDL_Surface`, whose fields an application reads directly.
const SURFACE_BYTES: u32 = 44;
const SURFACE_FLAGS: u32 = 0;
const SURFACE_FORMAT: u32 = 4;
const SURFACE_WIDTH: u32 = 8;
const SURFACE_HEIGHT: u32 = 12;
const SURFACE_PITCH: u32 = 16;
const SURFACE_PIXELS: u32 = 20;
const SURFACE_CLIP: u32 = 24;
const SURFACE_REFCOUNT: u32 = 40;

/// `SDL_PixelFormat`.
const FORMAT_BYTES: u32 = 40;
const FORMAT_PALETTE: u32 = 0;
const FORMAT_BITS: u32 = 4;
const FORMAT_BYTE_BITS: u32 = 5;
const FORMAT_R_LOSS: u32 = 6;
const FORMAT_R_SHIFT: u32 = 7;
const FORMAT_G_LOSS: u32 = 8;
const FORMAT_G_SHIFT: u32 = 9;
const FORMAT_B_LOSS: u32 = 10;
const FORMAT_B_SHIFT: u32 = 11;
const FORMAT_R_MASK: u32 = 16;
const FORMAT_G_MASK: u32 = 20;
const FORMAT_B_MASK: u32 = 24;
const FORMAT_A_MASK: u32 = 28;

/// `SDL_Palette` and `SDL_Color`.
const PALETTE_BYTES: u32 = 16;
const COLOR_BYTES: u32 = 4;
const PALETTE_ENTRIES: u32 = 256;

/// `SDL_AudioSpec`.
const AUDIO_SPEC_BYTES: u32 = 24;

/// The largest surface this host will hand out, in either direction. A game
/// asking for more is a corrupted mode, not a display.
const MAX_SIDE: u32 = 4096;

/// `SDL_Event` union members this shim answers.
const SDL_NOEVENT: u8 = 0;
const SDL_KEYDOWN: u8 = 2;
const SDL_KEYUP: u8 = 3;
const SDL_MOUSEMOTION: u8 = 4;
const SDL_MOUSEBUTTONDOWN: u8 = 5;
const SDL_MOUSEBUTTONUP: u8 = 6;
const SDL_QUIT: u8 = 12;

/// `SDL_KeyboardEvent`: `type`, `which`, `state`, padding, then the keysym.
const EVENT_BYTES: u32 = 20;
const KEY_SCANCODE: u32 = 4;
const KEY_SYM: u32 = 8;
const KEY_MOD: u32 = 12;

/// `KMOD_*` bits, which report which modifier keys were held.
const KMOD_LSHIFT: u32 = 0x0001;
const KMOD_RSHIFT: u32 = 0x0002;
const KMOD_LCTRL: u32 = 0x0040;
const KMOD_RCTRL: u32 = 0x0080;
const KMOD_LALT: u32 = 0x0100;
const KMOD_RALT: u32 = 0x0200;

/// SDL key codes for the keys that have no character.
const SDLK_BACKSPACE: u32 = 8;
const SDLK_TAB: u32 = 9;
const SDLK_RETURN: u32 = 13;
const SDLK_ESCAPE: u32 = 27;
const SDLK_SPACE: u32 = 32;
const SDLK_DELETE: u32 = 127;
const SDLK_UP: u32 = 273;
const SDLK_DOWN: u32 = 274;
const SDLK_RIGHT: u32 = 275;
const SDLK_LEFT: u32 = 276;
const SDLK_INSERT: u32 = 277;
const SDLK_HOME: u32 = 278;
const SDLK_END: u32 = 279;
const SDLK_PAGEUP: u32 = 280;
const SDLK_PAGEDOWN: u32 = 281;
const SDLK_F1: u32 = 282;
const SDLK_RSHIFT: u32 = 303;
const SDLK_LSHIFT: u32 = 304;
const SDLK_RCTRL: u32 = 305;
const SDLK_LCTRL: u32 = 306;
const SDLK_RALT: u32 = 307;
const SDLK_LALT: u32 = 308;

/// What `SDL_GetTicks` and the audio device remember.
#[derive(Default)]
pub struct Sdl {
    /// The `SDL_Surface` `SDL_SetVideoMode` last returned, 0 before the first.
    surface: u32,
    /// Its pixels in guest memory, released when the mode is replaced.
    pixels: Option<u32>,
    width: u32,
    height: u32,
    bits: u8,
    pitch: u32,
    /// `SDL_GetError`'s buffer, allocated on first use.
    error: u32,
    error_text: String,
    /// Where the pointer was at the previous motion event, for `xrel`/`yrel`.
    pointer: (i32, i32),
    /// Whether the hardware cursor is shown; SDL starts with it visible.
    cursor_shown: bool,
    /// The caption `SDL_WM_SetCaption` set, which the page may show.
    caption: String,
    /// The format `SDL_OpenAudio` was asked for, and whether it is paused.
    audio: Option<AudioSpec>,
    audio_paused: bool,
    /// `SDL_EnableUNICODE` and the key-repeat settings, which are configuration
    /// rather than behaviour here: text arrives through `WM_CHAR` already.
    unicode: bool,
}

/// The part of `SDL_AudioSpec` this host keeps.
#[derive(Clone, Copy)]
struct AudioSpec {
    frequency: u32,
    format: u32,
    channels: u32,
    samples: u32,
}

/// One field of a surface, as the guest wrote it.
struct Surface {
    width: u32,
    height: u32,
    pitch: u32,
    pixels: u32,
    bits: u8,
}

fn read_surface(call: &Call<'_>, address: u32) -> Result<Surface> {
    if address == 0 {
        return Err("SDL called with a null surface".to_string());
    }
    let format = call.memory.read_u32(address + SURFACE_FORMAT)?;
    let bits = if format == 0 {
        8
    } else {
        call.memory.read_u8(format + FORMAT_BITS)?
    };
    Ok(Surface {
        width: call.memory.read_u32(address + SURFACE_WIDTH)?,
        height: call.memory.read_u32(address + SURFACE_HEIGHT)?,
        pitch: u32::from(call.memory.read_u16(address + SURFACE_PITCH)?),
        pixels: call.memory.read_u32(address + SURFACE_PIXELS)?,
        bits,
    })
}

pub const SHIMS: &[Binding] = &[
    Binding { dll: SDL, name: "SDL_CloseAudio", arg_bytes: 0, shim: close_audio },
    Binding { dll: SDL, name: "SDL_Delay", arg_bytes: 4, shim: delay },
    Binding { dll: SDL, name: "SDL_EnableKeyRepeat", arg_bytes: 8, shim: enable_key_repeat },
    Binding { dll: SDL, name: "SDL_EnableUNICODE", arg_bytes: 4, shim: enable_unicode },
    Binding { dll: SDL, name: "SDL_FillRect", arg_bytes: 12, shim: fill_rect },
    Binding { dll: SDL, name: "SDL_Flip", arg_bytes: 4, shim: flip },
    Binding { dll: SDL, name: "SDL_GetError", arg_bytes: 0, shim: get_error },
    Binding { dll: SDL, name: "SDL_GetTicks", arg_bytes: 0, shim: get_ticks },
    Binding { dll: SDL, name: "SDL_Init", arg_bytes: 4, shim: init },
    Binding { dll: SDL, name: "SDL_InitSubSystem", arg_bytes: 4, shim: init_sub_system },
    Binding { dll: SDL, name: "SDL_JoystickEventState", arg_bytes: 4, shim: joystick_event_state },
    Binding { dll: SDL, name: "SDL_JoystickOpen", arg_bytes: 4, shim: joystick_open },
    Binding { dll: SDL, name: "SDL_MapRGB", arg_bytes: 16, shim: map_rgb },
    Binding { dll: SDL, name: "SDL_NumJoysticks", arg_bytes: 0, shim: num_joysticks },
    Binding { dll: SDL, name: "SDL_OpenAudio", arg_bytes: 8, shim: open_audio },
    Binding { dll: SDL, name: "SDL_PauseAudio", arg_bytes: 4, shim: pause_audio },
    Binding { dll: SDL, name: "SDL_PollEvent", arg_bytes: 4, shim: poll_event },
    Binding { dll: SDL, name: "SDL_Quit", arg_bytes: 0, shim: quit },
    Binding { dll: SDL, name: "SDL_QuitSubSystem", arg_bytes: 4, shim: quit_sub_system },
    Binding { dll: SDL, name: "SDL_SetModuleHandle", arg_bytes: 4, shim: set_module_handle },
    Binding { dll: SDL, name: "SDL_SetVideoMode", arg_bytes: 16, shim: set_video_mode },
    Binding { dll: SDL, name: "SDL_ShowCursor", arg_bytes: 4, shim: show_cursor },
    Binding { dll: SDL, name: "SDL_VideoModeOK", arg_bytes: 16, shim: video_mode_ok },
    Binding { dll: SDL, name: "SDL_WM_SetCaption", arg_bytes: 8, shim: set_caption },
    Binding { dll: SDL, name: "SDL_WaitEvent", arg_bytes: 4, shim: poll_event },
    Binding { dll: SDL, name: "SDL_putenv", arg_bytes: 4, shim: putenv },
];

// ---- lifetime ------------------------------------------------------------

fn init(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let flags = call.arg(0)?;
    let (video, audio) = (flags & 0x20 != 0, flags & 0x10 != 0);
    host.notice(format!(
        "SDL: initialised (video {}, audio {}, audio is not mixed yet)",
        if video { "asked for" } else { "unused" },
        if audio { "asked for" } else { "unused" }
    ));
    Ok(Return(0))
}

fn init_sub_system(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn quit(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.ddraw.unpublish();
    host.sdl.surface = 0;
    host.sdl.pixels = None;
    Ok(Return(0))
}

fn quit_sub_system(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if call.arg(0)? & 0x20 != 0 {
        host.ddraw.unpublish();
    }
    Ok(Return(0))
}

// ---- video ---------------------------------------------------------------

fn set_video_mode(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let width = call.arg(0)?;
    let height = call.arg(1)?;
    let requested = call.arg(2)?;
    let flags = call.arg(3)?;
    // 0 means "the best depth"; this host's answer is a truecolour one, because
    // that is what an SDL game of this era draws into without a palette.
    let bits = match requested {
        0 => 32,
        8 | 16 => requested as u8,
        // A 3-byte surface is accepted by SDL but nothing here reads one, so the
        // mode handed back is the 32-bit one the same pixels fit in.
        24 | 32 => 32,
        _ => {
            set_error(host, call, &format!("unsupported colour depth {requested}"))?;
            return Ok(Return(0));
        }
    };
    if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE {
        set_error(host, call, "the requested video mode is out of range")?;
        return Ok(Return(0));
    }

    let pitch = width * u32::from(bits / 8);
    let size = pitch * height;
    if let Some(previous) = host.sdl.pixels.take() {
        host.release(previous);
    }
    let pixels = match host.allocate(size, 16) {
        Ok(pixels) => pixels,
        Err(error) => {
            set_error(host, call, &format!("the video surface does not fit: {error}"))?;
            return Ok(Return(0));
        }
    };
    // The guest may draw only part of the surface before the first flip, and a
    // recycled block would otherwise show the previous session's frame.
    call.zero(pixels, size)?;

    let format = host.allocate(FORMAT_BYTES, 4)?;
    call.zero(format, FORMAT_BYTES)?;
    let (red, green, blue, alpha) = match bits {
        16 => (0xF800, 0x07E0, 0x001F, 0),
        24 | 32 => (0x00FF_0000, 0x0000_FF00, 0x0000_00FF, 0xFF00_0000),
        _ => (0, 0, 0, 0),
    };
    call.write(format + FORMAT_BITS, &[bits])?;
    call.write(format + FORMAT_BYTE_BITS, &[bits / 8])?;
    for (mask, at) in [(red, FORMAT_R_MASK), (green, FORMAT_G_MASK), (blue, FORMAT_B_MASK), (alpha, FORMAT_A_MASK)] {
        call.write_u32(format + at, mask)?;
    }
    if bits != 8 {
        for (mask, loss, shift) in [
            (red, FORMAT_R_LOSS, FORMAT_R_SHIFT),
            (green, FORMAT_G_LOSS, FORMAT_G_SHIFT),
            (blue, FORMAT_B_LOSS, FORMAT_B_SHIFT),
        ] {
            call.write(
                format + loss,
                &[8 - u8::try_from(mask.count_ones()).unwrap_or(8)],
            )?;
            call.write(
                format + shift,
                &[u8::try_from(mask.trailing_zeros()).unwrap_or(0)],
            )?;
        }
    } else {
        // An 8-bit surface needs a palette object even before anyone writes to
        // it: the application dereferences `format->palette` for its colour key.
        let palette = host.allocate(PALETTE_BYTES + PALETTE_ENTRIES * COLOR_BYTES, 4)?;
        call.zero(palette, PALETTE_BYTES + PALETTE_ENTRIES * COLOR_BYTES)?;
        call.write_u32(palette, PALETTE_ENTRIES)?;
        call.write_u32(palette + 4, palette + PALETTE_BYTES)?;
        call.write_u32(format + FORMAT_PALETTE, palette)?;
    }

    let surface = host.allocate(SURFACE_BYTES, 4)?;
    call.zero(surface, SURFACE_BYTES)?;
    call.write_u32(surface + SURFACE_FLAGS, flags)?;
    call.write_u32(surface + SURFACE_FORMAT, format)?;
    call.write_u32(surface + SURFACE_WIDTH, width)?;
    call.write_u32(surface + SURFACE_HEIGHT, height)?;
    call.write_u16(surface + SURFACE_PITCH, pitch as u16)?;
    call.write_u32(surface + SURFACE_PIXELS, pixels)?;
    call.write_u16(surface + SURFACE_CLIP, 0)?;
    call.write_u16(surface + SURFACE_CLIP + 2, 0)?;
    call.write_u16(surface + SURFACE_CLIP + 4, width as u16)?;
    call.write_u16(surface + SURFACE_CLIP + 6, height as u16)?;
    call.write_u32(surface + SURFACE_REFCOUNT, 1)?;

    host.ddraw.publish(pixels, width, height, pitch, bits);
    host.sdl.surface = surface;
    host.sdl.pixels = Some(pixels);
    host.sdl.width = width;
    host.sdl.height = height;
    host.sdl.bits = bits;
    host.sdl.pitch = pitch;
    host.notice(format!("SDL: video mode {width}x{height}x{bits}"));
    Ok(Return(surface))
}

fn video_mode_ok(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let bits = call.arg(2)?;
    Ok(Return(match bits {
        0 | 8 | 16 | 24 | 32 => bits,
        _ => 0,
    }))
}

fn flip(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // Nothing is copied: the guest's own pixels are the frame, and the counter is
    // what tells a page they changed.
    host.ddraw.present();
    Ok(Return(0))
}

/// `SDL_FillRect(dst, rect, color)`: a solid run per row, clipped to the
/// surface. The rectangle and the colour are in the surface's own format.
fn fill_rect(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let address = match call.arg(0)? {
        0 => host.sdl.surface,
        address => address,
    };
    if address == 0 {
        set_error(host, call, "SDL_FillRect before SDL_SetVideoMode")?;
        return Ok(Return(-1i32 as u32));
    }
    let surface = read_surface(call, address)?;
    if surface.pixels == 0 {
        return Ok(Return(0));
    }
    let rect = call.arg(1)?;
    let (mut x, mut y, mut width, mut height) = if rect == 0 {
        (
            i32::from(call.memory.read_u16(address + SURFACE_CLIP)?),
            i32::from(call.memory.read_u16(address + SURFACE_CLIP + 2)?),
            u32::from(call.memory.read_u16(address + SURFACE_CLIP + 4)?),
            u32::from(call.memory.read_u16(address + SURFACE_CLIP + 6)?),
        )
    } else {
        (
            i32::from(call.memory.read_u16(rect)? as i16),
            i32::from(call.memory.read_u16(rect + 2)? as i16),
            u32::from(call.memory.read_u16(rect + 4)?),
            u32::from(call.memory.read_u16(rect + 6)?),
        )
    };
    if x < 0 {
        width = width.saturating_sub((-x) as u32);
        x = 0;
    }
    if y < 0 {
        height = height.saturating_sub((-y) as u32);
        y = 0;
    }
    if width == 0 || height == 0 {
        return Ok(Return(0));
    }
    let x = x as u32;
    let y = y as u32;
    let width = width.min(surface.width.saturating_sub(x));
    let height = height.min(surface.height.saturating_sub(y));
    let bytes = u32::from(surface.bits / 8);
    let colour = call.arg(2)?;
    let mut row = Vec::with_capacity((width * bytes) as usize);
    for _ in 0..width {
        row.extend_from_slice(&colour.to_le_bytes()[..bytes as usize]);
    }
    for line in 0..height {
        let at = surface.pixels + (y + line) * surface.pitch + x * bytes;
        call.write(at, &row)?;
    }
    Ok(Return(0))
}

/// `SDL_MapRGB(format, r, g, b)`: the format's masks decide the packing, which is
/// what lets one game draw into 565 and 888 surfaces with the same code.
fn map_rgb(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let format = call.arg(0)?;
    let (red, green, blue) = (call.arg(1)?, call.arg(2)?, call.arg(3)?);
    if format == 0 {
        return Ok(Return((red << 16) | (green << 8) | blue));
    }
    let mut value = 0;
    for (component, at) in [(red, FORMAT_R_MASK), (green, FORMAT_G_MASK), (blue, FORMAT_B_MASK)] {
        let mask = call.memory.read_u32(format + at)?;
        if mask == 0 {
            continue;
        }
        let shift = mask.trailing_zeros();
        let bits = mask.count_ones();
        value |= (component >> (8 - bits) << shift) & mask;
    }
    Ok(Return(value))
}

fn set_caption(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.sdl.caption = call.cstr(0)?;
    Ok(Return(0))
}

// ---- events --------------------------------------------------------------

fn poll_event(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(0)?;
    if destination == 0 {
        return Ok(Return(0));
    }
    while let Some(message) = host.input.next() {
        call.zero(destination, EVENT_BYTES)?;
        match message.message {
            input::WM_KEYDOWN | input::WM_KEYUP => {
                let down = message.message == input::WM_KEYDOWN;
                let scancode = ((message.lparam >> 16) & 0xff) as u8;
                call.write(destination, &[if down { SDL_KEYDOWN } else { SDL_KEYUP }])?;
                call.write(destination + 2, &[u8::from(down)])?;
                call.write(destination + KEY_SCANCODE, &[scancode])?;
                call.write_u32(destination + KEY_SYM, keysym(message.wparam as u8, scancode))?;
                call.write_u32(destination + KEY_MOD, modifiers(&host.input))?;
                return Ok(Return(1));
            }
            input::WM_MOUSEMOVE => {
                let relative = motion(host, message);
                call.write(destination, &[SDL_MOUSEMOTION])?;
                call.write(destination + 2, &[host.input.button_state()])?;
                call.write_u16(destination + 4, message.x as u16)?;
                call.write_u16(destination + 6, message.y as u16)?;
                call.write_u16(destination + 8, relative.0 as u16)?;
                call.write_u16(destination + 10, relative.1 as u16)?;
                return Ok(Return(1));
            }
            input::WM_LBUTTONDOWN | input::WM_LBUTTONUP | input::WM_RBUTTONDOWN
            | input::WM_RBUTTONUP | input::WM_MBUTTONDOWN | input::WM_MBUTTONUP => {
                let down = matches!(
                    message.message,
                    input::WM_LBUTTONDOWN | input::WM_RBUTTONDOWN | input::WM_MBUTTONDOWN
                );
                let button = match message.message {
                    input::WM_LBUTTONDOWN | input::WM_LBUTTONUP => 1,
                    input::WM_MBUTTONDOWN | input::WM_MBUTTONUP => 2,
                    _ => 3,
                };
                call.write(
                    destination,
                    &[if down { SDL_MOUSEBUTTONDOWN } else { SDL_MOUSEBUTTONUP }],
                )?;
                call.write(destination + 1, &[0])?;
                call.write(destination + 2, &[button])?;
                call.write(destination + 3, &[u8::from(down)])?;
                call.write_u16(destination + 4, message.x as u16)?;
                call.write_u16(destination + 6, message.y as u16)?;
                return Ok(Return(1));
            }
            input::WM_QUIT => {
                call.write(destination, &[SDL_QUIT])?;
                return Ok(Return(1));
            }
            _ => continue,
        }
    }
    call.write(destination, &[SDL_NOEVENT])?;
    Ok(Return(0))
}

/// The movement since the previous motion event, which SDL reports alongside the
/// absolute position.
fn motion(host: &mut Host, message: Message) -> (i32, i32) {
    let relative = (message.x - host.sdl.pointer.0, message.y - host.sdl.pointer.1);
    host.sdl.pointer = (message.x, message.y);
    relative
}

fn modifiers(input: &input::Input) -> u32 {
    let held = input.key_state();
    let mut modifiers = 0;
    if held[0x2a] != 0 {
        modifiers |= KMOD_LSHIFT;
    }
    if held[0x36] != 0 {
        modifiers |= KMOD_RSHIFT;
    }
    if held[0x1d] != 0 {
        modifiers |= KMOD_LCTRL;
    }
    if held[0x9d] != 0 {
        modifiers |= KMOD_RCTRL;
    }
    if held[0x38] != 0 {
        modifiers |= KMOD_LALT;
    }
    if held[0xb8] != 0 {
        modifiers |= KMOD_RALT;
    }
    modifiers
}

/// The SDL key code for a virtual key. Letters and digits are their lowercase
/// ASCII, which is what SDL reports whatever the shift state; the rest are the
/// `SDLK_*` numbers above 255.
fn keysym(key: u8, scancode: u8) -> u32 {
    match key {
        0x08 => SDLK_BACKSPACE,
        0x09 => SDLK_TAB,
        0x0d => SDLK_RETURN,
        0x1b => SDLK_ESCAPE,
        0x20 => SDLK_SPACE,
        0x23 => SDLK_END,
        0x24 => SDLK_HOME,
        0x25 => SDLK_LEFT,
        0x26 => SDLK_UP,
        0x27 => SDLK_RIGHT,
        0x28 => SDLK_DOWN,
        0x21 => SDLK_PAGEUP,
        0x22 => SDLK_PAGEDOWN,
        0x2d => SDLK_INSERT,
        0x2e => SDLK_DELETE,
        0x30..=0x39 => u32::from(key),
        0x41..=0x5a => u32::from(key.to_ascii_lowercase()),
        0x60..=0x69 => u32::from(b'0' + (key - 0x60)),
        0x70..=0x7b => SDLK_F1 + u32::from(key - 0x70),
        0xa0 => SDLK_LSHIFT,
        0xa1 => SDLK_RSHIFT,
        0xa2 => SDLK_LCTRL,
        0xa3 => SDLK_RCTRL,
        0xa4 => SDLK_LALT,
        0xa5 => SDLK_RALT,
        _ => u32::from(scancode),
    }
}

fn enable_key_repeat(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The page repeats a held key, which is the same autorepeat the setting asks
    // for; nothing here has to remember the delay.
    Ok(Return(0))
}

fn enable_unicode(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let previous = host.sdl.unicode;
    host.sdl.unicode = call.arg(0)? != 0;
    Ok(Return(u32::from(previous)))
}

fn show_cursor(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let previous = host.sdl.cursor_shown;
    host.sdl.cursor_shown = call.arg(0)? != 0;
    Ok(Return(u32::from(previous)))
}

// ---- clock and environment ----------------------------------------------

fn get_ticks(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The page pushes its own clock through `wh_set_time`, which is what makes a
    // game's timers follow real time instead of the interpreter's speed.
    Ok(Return(host.now_ms() as u32))
}

fn delay(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The frame clock owns pacing; sleeping here would stall the page.
    Ok(Return(0))
}

fn putenv(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn set_module_handle(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn num_joysticks(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn joystick_open(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.sdl.error_text = "no joystick is attached".to_string();
    Ok(Return(0))
}

fn joystick_event_state(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // SDL_IGNORE: the events are not generated, and this host has no joystick.
    Ok(Return(1))
}

fn get_error(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = host.sdl.error_text.clone();
    let buffer = ensure_error_buffer(host, call)?;
    let clipped = &text.as_bytes()[..text.len().min(200)];
    call.write(buffer, clipped)?;
    call.write(buffer + clipped.len() as u32, &[0])?;
    Ok(Return(buffer))
}

fn ensure_error_buffer(host: &mut Host, call: &mut Call<'_>) -> Result<u32> {
    if host.sdl.error != 0 {
        return Ok(host.sdl.error);
    }
    let buffer = host.allocate(256, 4)?;
    call.zero(buffer, 256)?;
    host.sdl.error = buffer;
    Ok(buffer)
}

fn set_error(host: &mut Host, call: &mut Call<'_>, text: &str) -> Result<()> {
    host.sdl.error_text = text.to_string();
    let buffer = ensure_error_buffer(host, call)?;
    let clipped = &text.as_bytes()[..text.len().min(200)];
    call.write(buffer, clipped)?;
    call.write(buffer + clipped.len() as u32, &[0])?;
    Ok(())
}

// ---- audio ---------------------------------------------------------------

fn open_audio(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let desired = call.arg(0)?;
    let obtained = call.arg(1)?;
    if desired == 0 {
        set_error(host, call, "SDL_OpenAudio without a spec")?;
        return Ok(Return(-1i32 as u32));
    }
    let spec = AudioSpec {
        frequency: call.memory.read_u32(desired)?,
        format: u32::from(call.memory.read_u16(desired + 4)?),
        channels: u32::from(call.memory.read_u8(desired + 6)?),
        samples: u32::from(call.memory.read_u16(desired + 8)?),
    };
    if spec.frequency == 0 || spec.channels == 0 {
        set_error(host, call, "SDL_OpenAudio with an unusable spec")?;
        return Ok(Return(-1i32 as u32));
    }
    // The device is accepted at the format asked for, so a game that treats sound
    // as optional keeps running. Nothing mixes the callback yet, and the notice
    // says so rather than letting the silence read as a game with no sound.
    if obtained != 0 {
        let bytes = call.memory.read_bytes(desired, AUDIO_SPEC_BYTES as usize)?.to_vec();
        call.write(obtained, &bytes)?;
    }
    host.sdl.audio = Some(spec);
    host.sdl.audio_paused = false;
    host.notice(format!(
        "SDL: audio opened at {} Hz, type {:#06x}, {} channel(s), {} samples (no mixer yet)",
        spec.frequency, spec.format, spec.channels, spec.samples
    ));
    Ok(Return(0))
}

fn close_audio(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    host.sdl.audio = None;
    Ok(Return(0))
}

fn pause_audio(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.sdl.audio_paused = call.arg(0)? != 0;
    Ok(Return(0))
}
