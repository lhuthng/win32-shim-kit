//! `WINMM.dll`: the multimedia devices this host does not have.
//!
//! The game's music comes from the CD through `mciSendCommandA`, its volume from
//! the mixer, and its animations from `timeSetEvent`. None of that exists in a
//! page yet, and the truthful answer to all of it is "no device": the guest keeps
//! its own no-sound path (it already took it when `DirectSoundCreate` failed) and
//! the boot continues.
//!
//! What is implemented here is deliberately only the refusal:
//!
//! - `mciSendCommandA` returns `MCIERR_DEVICE_NOT_INSTALLED` for every command.
//!   The M5 audio work replaces this with a stream over the converted `BGM.dat`.
//! - The mixer, aux and joystick queries answer `MMSYSERR_NODRIVER` / zero
//!   devices, which is what a machine without those devices says.
//! - `timeGetDevCaps` reports a 1 ms timer resolution and `timeBeginPeriod`
//!   succeeds, because both are statements about the clock the page already has.
//!
//! # Timers
//!
//! `timeSetEvent` is real: it hands out an id and the session calls the guest's
//! callback on a fixed instruction interval. The game uses it for its animation
//! clock -- the fade-in before the title screen waits inside
//! `while (fade != done);` and only the timer moves the fade along -- so a "no
//! timer" answer left the guest spinning for ever.
//!
//! The interval is *milliseconds*, and the session asks the host for the guest's
//! time when it decides whether a callback is due. An embedder that has a clock
//! supplies it ([`crate::shim::Host::set_clock`]), and the page does, with
//! `performance.now()`, before every chunk: a 33 ms animation timer then fires
//! every 33 ms of the visitor's time, which is the rate the game was drawn for.
//!
//! Timing on the instruction count instead makes animation speed a property of
//! this interpreter's throughput -- at 15 M instructions a second the same timer
//! fires 45 times a second, and every animated thing in the game moves faster and
//! further than it does on the machine it came from. That exchange rate is still
//! the fallback for a run with no clock ([`INSTRUCTIONS_PER_MILLISECOND`], a 10 M
//! instructions per second guest), because a headless run has to be repeatable.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const WINMM: &str = "WINMM.dll";

const MMSYSERR_NODRIVER: u32 = 6;
const JOYERR_UNPLUGGED: u32 = 167;
/// `MCIERR_BASE` (256) + 18: no MCI device is installed.
const MCIERR_DEVICE_NOT_INSTALLED: u32 = 274;

/// `MMIOERR_BASE` (256) + offsets: the failures `mmioOpenA` and friends report.
const MMIOERR_FILENOTFOUND: u32 = 257;
const MMIOERR_CHUNKNOTFOUND: u32 = 264;

/// `mmioOpenA` flags. Writing is not offered: a guest that wants to write a
/// RIFF gets `0`, the same answer Windows gives for a device it cannot open.
const MMIO_WRITE: u32 = 0x0001;
const MMIO_READWRITE: u32 = 0x0002;

/// `mmioDescend` flags: which kind of chunk to stop at.
const MMIO_FINDCHUNK: u32 = 0x0010;
const MMIO_FINDRIFF: u32 = 0x0020;
const MMIO_FINDLIST: u32 = 0x0040;

/// `MMCKINFO`: the chunk id, its size, the form type, where its data starts and
/// the flags.
const MMCK_ID: u32 = 0;
const MMCK_SIZE: u32 = 4;
const MMCK_TYPE: u32 = 8;
const MMCK_OFFSET: u32 = 12;
const MMCK_FLAGS: u32 = 16;

pub const SHIMS: &[Binding] = &[
    Binding { dll: WINMM, name: "auxGetDevCapsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "auxGetNumDevs", arg_bytes: 0, shim: no_devices },
    Binding { dll: WINMM, name: "auxGetVolume", arg_bytes: 8, shim: no_driver },
    Binding { dll: WINMM, name: "joyGetDevCapsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "joyGetNumDevs", arg_bytes: 0, shim: no_devices },
    Binding { dll: WINMM, name: "joyGetPosEx", arg_bytes: 8, shim: joystick_unplugged },
    Binding { dll: WINMM, name: "joySetCapture", arg_bytes: 16, shim: joystick_unplugged },
    Binding { dll: WINMM, name: "joySetThreshold", arg_bytes: 8, shim: joystick_unplugged },
    Binding { dll: WINMM, name: "mmioOpenA", arg_bytes: 12, shim: mmio_open },
    Binding { dll: WINMM, name: "mmioClose", arg_bytes: 8, shim: mmio_close },
    Binding { dll: WINMM, name: "mmioDescend", arg_bytes: 16, shim: mmio_descend },
    Binding { dll: WINMM, name: "mmioAscend", arg_bytes: 12, shim: mmio_ascend },
    Binding { dll: WINMM, name: "mmioRead", arg_bytes: 12, shim: mmio_read },
    Binding { dll: WINMM, name: "mciSendCommandA", arg_bytes: 16, shim: mci_send_command },
    Binding { dll: WINMM, name: "mciSendStringA", arg_bytes: 16, shim: mci_send_string },
    Binding { dll: WINMM, name: "mixerGetControlDetailsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerGetDevCapsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerGetLineControlsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerGetLineInfoA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerSetControlDetails", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "timeBeginPeriod", arg_bytes: 4, shim: time_begin_period },
    Binding { dll: WINMM, name: "waveOutGetDevCapsA", arg_bytes: 12, shim: wave_out_get_dev_caps },
    Binding { dll: WINMM, name: "timeGetDevCaps", arg_bytes: 8, shim: time_get_dev_caps },
    Binding { dll: WINMM, name: "timeGetTime", arg_bytes: 0, shim: time_get_time },
    Binding { dll: WINMM, name: "timeEndPeriod", arg_bytes: 4, shim: time_begin_period },
    Binding { dll: WINMM, name: "timeKillEvent", arg_bytes: 4, shim: time_kill_event },
    Binding { dll: WINMM, name: "timeSetEvent", arg_bytes: 20, shim: time_set_event },
];

fn no_driver(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(MMSYSERR_NODRIVER))
}

fn no_devices(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn joystick_unplugged(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(JOYERR_UNPLUGGED))
}

fn mci_send_command(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    if !host
        .notices
        .iter()
        .any(|notice| notice.starts_with("mciSendCommandA"))
    {
        host.notice("mciSendCommandA: no MCI device installed (no CD audio)".to_string());
    }
    Ok(Return(MCIERR_DEVICE_NOT_INSTALLED))
}

/// `mciSendStringA(command, &return, length, hwnd)`: the string form of MCI, which
/// a game uses to play a MIDI or CD track.
///
/// There is no MCI device in this machine, and the answer says so rather than
/// accepting the command: a score that thinks its music is playing cannot tell
/// the difference, while one that falls back to silence reports it.
fn mci_send_string(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    if !host.notices.iter().any(|notice| notice.starts_with("mciSendStringA")) {
        host.notice("mciSendStringA: no MCI device installed (MIDI and CD audio are silent)".to_string());
    }
    Ok(Return(MCIERR_DEVICE_NOT_INSTALLED))
}

/// `waveOutGetDevCapsA(device, caps, size)`: one device, stereo, 16-bit.
///
/// A game asks this to decide whether to play sound at all, and the answer here
/// is the mixer this host actually offers through DirectSound. The name is what
/// a settings screen would show, so it says where the sound comes from.
fn wave_out_get_dev_caps(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    const CAPS_BYTES: u32 = 52;
    let caps = call.arg(1)?;
    let size = call.arg(2)?;
    if caps != 0 && size != 0 {
        let mut bytes = vec![0_u8; CAPS_BYTES.min(size) as usize];
        let name = b"Browser audio";
        bytes[0..2].copy_from_slice(&1_u16.to_le_bytes()); // wMid
        bytes[2..4].copy_from_slice(&1_u16.to_le_bytes()); // wPid
        bytes[4..8].copy_from_slice(&0x0100_u32.to_le_bytes()); // vDriverVersion
        for (index, byte) in name.iter().enumerate() {
            if 8 + index < bytes.len() {
                bytes[8 + index] = *byte;
            }
        }
        if bytes.len() >= 44 {
            // WAVE_FORMAT_1M16 | WAVE_FORMAT_1S16 | WAVE_FORMAT_2S16
            bytes[40..44].copy_from_slice(&0x0014_u32.to_le_bytes());
        }
        if bytes.len() >= 48 {
            bytes[44..46].copy_from_slice(&2_u16.to_le_bytes()); // wChannels
        }
        call.write(caps, &bytes)?;
    }
    Ok(Return(0))
}

/// `timeBeginPeriod` and `timeEndPeriod` both succeed: the clock this host serves
/// already has millisecond resolution, so there is no period to widen or restore.
fn time_begin_period(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // The page's clock is already finer than a millisecond; there is no timer
    // resolution to raise.
    Ok(Return(0))
}

fn time_get_dev_caps(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let caps = call.arg(0)?;
    if caps != 0 {
        call.write_u32(caps, 1)?; // wPeriodMin: 1 ms
        call.write_u32(caps.wrapping_add(4), 1000)?; // wPeriodMax
    }
    Ok(Return(0))
}

/// A file open through `mmioOpenA`.
#[derive(Default)]
pub struct Mmio {
    open: std::collections::HashMap<u32, MmioFile>,
    next: u32,
}

struct MmioFile {
    path: String,
    position: u32,
}

impl Mmio {
    pub fn open(&mut self, path: &str) -> u32 {
        self.next += 1;
        // Above the file, search and heap handles the other tables hand out, so a
        // log line says which table a number belongs to.
        let handle = 0x0009_0000 + self.next * 4;
        self.open.insert(
            handle,
            MmioFile {
                path: path.to_string(),
                position: 0,
            },
        );
        handle
    }

    fn path(&self, handle: u32) -> Option<&str> {
        self.open.get(&handle).map(|file| file.path.as_str())
    }

    fn position(&self, handle: u32) -> Option<u32> {
        self.open.get(&handle).map(|file| file.position)
    }

    fn seek(&mut self, handle: u32, position: u32) {
        if let Some(file) = self.open.get_mut(&handle) {
            file.position = position;
        }
    }

    fn close(&mut self, handle: u32) -> bool {
        self.open.remove(&handle).is_some()
    }

    pub fn open_count(&self) -> usize {
        self.open.len()
    }
}

/// `mmioOpenA(path, info, flags)`: a handle for reading a file through `mmio`.
///
/// The file has to be one the guest was given, because there is no disk here:
/// the table is the only place a name resolves. A write request is refused with
/// `0`, which is what a game handles by not writing.
fn mmio_open(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let path = call.arg(0)?;
    let flags = call.arg(2)?;
    if path == 0 {
        // A memory file with an `MMIOINFO` describing it: nothing in the pool has
        // needed one yet, and inventing a buffer would hide that.
        return Ok(Return(0));
    }
    let path = call.memory.read_cstr(path)?;
    if flags & (MMIO_WRITE | MMIO_READWRITE) != 0 {
        if !host.notices.iter().any(|notice| notice.starts_with("mmioOpenA")) {
            host.notice(format!(
                "mmioOpenA {path}: this host opens files for reading, so a RIFF write is refused"
            ));
        }
        return Ok(Return(0));
    }
    if !host.files.exists(&path) {
        return Ok(Return(0));
    }
    let handle = host.mmio.open(&path);
    Ok(Return(handle))
}

fn mmio_close(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.mmio.close(call.arg(0)?);
    // `MMSYSERR_NOERROR`: a close either happened or the handle was not one.
    Ok(Return(0))
}

/// `mmioDescend(hmmio, &chunk, &parent, flags)`: finds a chunk and fills in where
/// its data starts.
///
/// Four bytes of id and four of size, with `RIFF` and `LIST` carrying a form type
/// of their own, is the whole of the format. `MMIO_FINDCHUNK` looks for the id the
/// caller put in the structure, `MMIO_FINDRIFF` and `MMIO_FINDLIST` for a form of
/// that kind, and no flag at all takes whatever is next -- which is how a guest
/// walks a file it already knows.
fn mmio_descend(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let chunk = call.arg(1)?;
    let parent = call.arg(2)?;
    let flags = call.arg(3)?;
    let Some(path) = host.mmio.path(handle).map(str::to_string) else {
        return Ok(Return(MMIOERR_FILENOTFOUND));
    };
    let Some(bytes) = host.files.contents(&path).map(<[u8]>::to_vec) else {
        return Ok(Return(MMIOERR_FILENOTFOUND));
    };
    let position = host.mmio.position(handle).unwrap_or(0);
    let (mut at, limit) = if parent != 0 {
        let offset = call.memory.read_u32(parent + MMCK_OFFSET)?;
        let size = call.memory.read_u32(parent + MMCK_SIZE)?;
        (offset, offset.saturating_add(size).min(bytes.len() as u32))
    } else {
        (position, bytes.len() as u32)
    };
    let wanted = if flags & MMIO_FINDCHUNK != 0 {
        Some(call.memory.read_u32(chunk)?)
    } else {
        None
    };
    loop {
        if at.saturating_add(8) > limit {
            return Ok(Return(MMIOERR_CHUNKNOTFOUND));
        }
        let id = read_fourcc(&bytes, at);
        let size = u32::from_le_bytes(
            bytes[at as usize + 4..at as usize + 8]
                .try_into()
                .expect("four bytes"),
        );
        let form = matches!(&id, b"RIFF" | b"LIST");
        let matched = match wanted {
            Some(wanted) => wanted == u32::from_le_bytes(id),
            None if flags & MMIO_FINDRIFF != 0 => &id == b"RIFF",
            None if flags & MMIO_FINDLIST != 0 => &id == b"LIST",
            None => true,
        };
        if matched {
            let kind = if form {
                u32::from_le_bytes(read_fourcc(&bytes, at + 8))
            } else {
                0
            };
            let offset = at + 8 + u32::from(form) * 4;
            call.write_u32(chunk + MMCK_ID, u32::from_le_bytes(id))?;
            call.write_u32(chunk + MMCK_SIZE, size)?;
            call.write_u32(chunk + MMCK_TYPE, kind)?;
            call.write_u32(chunk + MMCK_OFFSET, offset)?;
            call.write_u32(chunk + MMCK_FLAGS, 0)?;
            host.mmio.seek(handle, offset);
            return Ok(Return(0));
        }
        // Chunks are word-aligned: an odd size is followed by one pad byte.
        at = at
            .saturating_add(8)
            .saturating_add(size)
            .saturating_add(size & 1);
    }
}

/// `mmioAscend(hmmio, &chunk, flags)`: moves past the chunk's data.
fn mmio_ascend(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let chunk = call.arg(1)?;
    let offset = call.memory.read_u32(chunk + MMCK_OFFSET)?;
    let size = call.memory.read_u32(chunk + MMCK_SIZE)?;
    host.mmio
        .seek(handle, offset.saturating_add(size).saturating_add(size & 1));
    Ok(Return(0))
}

/// `mmioRead(hmmio, buffer, count)`: bytes from the file's cursor, `-1` for a
/// handle that is not open and `0` at the end.
fn mmio_read(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let buffer = call.arg(1)?;
    let count = call.arg(2)?;
    let Some(path) = host.mmio.path(handle).map(str::to_string) else {
        return Ok(Return(u32::MAX));
    };
    let position = host.mmio.position(handle).unwrap_or(0);
    let Some(bytes) = host.files.contents(&path) else {
        return Ok(Return(u32::MAX));
    };
    let start = (position as usize).min(bytes.len());
    let end = (start + count as usize).min(bytes.len());
    let chunk = bytes[start..end].to_vec();
    call.write(buffer, &chunk)?;
    host.mmio.seek(handle, end as u32);
    Ok(Return(chunk.len() as u32))
}

fn read_fourcc(bytes: &[u8], at: u32) -> [u8; 4] {
    let at = at as usize;
    if at + 4 > bytes.len() {
        return [0; 4];
    }
    [bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]
}

/// Instructions the host attributes to one millisecond of guest time, for a run
/// with no clock of its own. See the module note: with an embedder clock this is
/// not used at all.
pub const INSTRUCTIONS_PER_MILLISECOND: u64 = 10_000;

/// `TIME_PERIODIC`: fire again after each interval. `TIME_ONESHOT` (0) fires once.
const TIME_PERIODIC: u32 = 1;

/// One callback the guest asked for.
#[derive(Clone, Copy, Debug)]
pub struct Timer {
    pub id: u32,
    /// Guest address of the callback, `void CALLBACK (UINT, UINT, DWORD_PTR,
    /// DWORD_PTR, DWORD_PTR)`.
    pub callback: u32,
    /// The `dwUser` value the guest passed and expects back as argument 2.
    pub user: u32,
    /// Milliseconds between firings.
    pub every_ms: u64,
    pub periodic: bool,
    /// The guest time, in milliseconds, the next firing is due at. Zero until
    /// the first look schedules it, because a shim does not know the clock.
    pub due_ms: u64,
}

/// The guest's timers, in the order they were set.
#[derive(Default)]
pub struct Timers {
    entries: Vec<Timer>,
    next_id: u32,
}

impl Timers {
    pub fn count(&self) -> usize {
        self.entries.len()
    }

    pub fn set(&mut self, delay_ms: u32, callback: u32, user: u32, periodic: bool) -> u32 {
        self.next_id += 1;
        let id = self.next_id;
        self.entries.push(Timer {
            id,
            callback,
            user,
            every_ms: u64::from(delay_ms.max(1)),
            periodic,
            due_ms: 0,
        });
        id
    }

    pub fn kill(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|timer| timer.id != id);
        before != self.entries.len()
    }

    /// The callback due at guest time `now_ms`, if any, rescheduled for its next
    /// firing. Timers are scheduled on first look, since only the session knows
    /// the clock.
    pub fn due(&mut self, now_ms: u64) -> Option<(u32, u32, u32)> {
        for timer in &mut self.entries {
            if timer.due_ms == 0 {
                timer.due_ms = now_ms + timer.every_ms;
            }
        }
        let (index, _) = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, timer)| timer.due_ms <= now_ms)
            .min_by_key(|(_, timer)| timer.due_ms)?;
        let timer = self.entries[index];
        if timer.periodic {
            // Skip the ticks that passed while the guest was busy, so a long run
            // does not fire a burst of callbacks for one interval.
            let behind = (now_ms - timer.due_ms) / timer.every_ms + 1;
            self.entries[index].due_ms = timer.due_ms + behind * timer.every_ms;
        } else {
            self.entries.remove(index);
        }
        Some((timer.id, timer.callback, timer.user))
    }
}

/// `timeGetTime()`: the guest's clock in milliseconds, counting from the session's
/// start. The page supplies it, so a game that measures a fade against it follows
/// real time rather than how fast the interpreter happens to run.
fn time_get_time(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.now_ms() as u32))
}

fn time_set_event(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let delay = call.arg(0)?;
    let callback = call.arg(2)?;
    let user = call.arg(3)?;
    let event = call.arg(4)?;
    if callback == 0 || delay == 0 {
        // Nothing to call: this is "no timer", not an error, and the guest sees
        // the same 0 it would get from a device without timer support.
        return Ok(Return(0));
    }
    let periodic = event & TIME_PERIODIC != 0;
    let id = host.timers.set(delay, callback, user, periodic);
    host.notice(format!(
        "timeSetEvent: {delay} ms {} timer #{id} calling {callback:#010x}",
        if periodic { "periodic" } else { "one-shot" }
    ));
    Ok(Return(id))
}

fn time_kill_event(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let id = call.arg(0)?;
    // Killing an id nothing owns succeeds, which is what Windows does for 0.
    host.timers.kill(id);
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
        let args = STACK_BASE + 0xa000;
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
    fn mmio_walks_a_riff_file_and_reads_a_chunk() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // What a game opens to learn a wave file's format: a `RIFF` form of type
        // `WAVE` holding one four-byte `fmt ` chunk.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&4_u32.to_le_bytes());
        wav.extend_from_slice(&[1, 2, 3, 4]);
        host.files.provide("song.wav", wav);

        let name = STACK_BASE + 0x2000;
        memory.write_bytes(name, b"song.wav\0").unwrap();
        let Return(handle) =
            call_shim(&mut host, &mut memory, "mmioOpenA", &[name, 0, 0]).unwrap()
        else {
            panic!("mmioOpenA answered something other than a handle");
        };
        assert_ne!(handle, 0, "a provided file opens");
        assert_eq!(host.mmio.open_count(), 1);

        // The form itself: its id, its size, its type and where its data starts.
        let form = STACK_BASE + 0x2100;
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "mmioDescend",
                &[handle, form, 0, MMIO_FINDRIFF]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(memory.read_bytes(form + MMCK_ID, 4).unwrap(), b"RIFF");
        assert_eq!(memory.read_u32(form + MMCK_SIZE).unwrap(), 16);
        assert_eq!(
            memory.read_u32(form + MMCK_TYPE).unwrap(),
            u32::from_le_bytes(*b"WAVE")
        );
        assert_eq!(memory.read_u32(form + MMCK_OFFSET).unwrap(), 12);

        // The chunk inside it, found by the id the caller put in the structure.
        let chunk = STACK_BASE + 0x2200;
        memory
            .write_u32(chunk + MMCK_ID, u32::from_le_bytes(*b"fmt "))
            .unwrap();
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "mmioDescend",
                &[handle, chunk, form, MMIO_FINDCHUNK]
            )
            .unwrap(),
            Return(0)
        );
        assert_eq!(memory.read_u32(chunk + MMCK_SIZE).unwrap(), 4);
        assert_eq!(memory.read_u32(chunk + MMCK_OFFSET).unwrap(), 20);

        let buffer = STACK_BASE + 0x2300;
        assert_eq!(
            call_shim(&mut host, &mut memory, "mmioRead", &[handle, buffer, 4]).unwrap(),
            Return(4)
        );
        assert_eq!(memory.read_bytes(buffer, 4).unwrap(), &[1, 2, 3, 4]);
        assert_eq!(
            call_shim(&mut host, &mut memory, "mmioRead", &[handle, buffer, 4]).unwrap(),
            Return(0),
            "the end of the file is not an error"
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "mmioClose", &[handle, 0]).unwrap(),
            Return(0)
        );
        assert_eq!(host.mmio.open_count(), 0);
    }

    #[test]
    fn there_is_no_joystick_to_capture() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "joyGetNumDevs", &[]).unwrap(),
            Return(0)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "joySetCapture", &[0, 0, 0, 0]).unwrap(),
            Return(JOYERR_UNPLUGGED)
        );
    }

    #[test]
    fn the_wave_device_reports_stereo_sixteen_bit_within_the_callers_size() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let caps = STACK_BASE + 0x3000;
        memory.write_bytes(caps, &[0xaa; 64]).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "waveOutGetDevCapsA", &[0, caps, 52]).unwrap(),
            Return(0)
        );
        let name = memory.read_bytes(caps + 8, 14).unwrap();
        assert_eq!(&name[..13], b"Browser audio");
        assert_eq!(name[13], 0, "the name is terminated");
        assert_eq!(memory.read_u32(caps + 40).unwrap(), 0x14, "16-bit formats");
        assert_eq!(memory.read_u16(caps + 44).unwrap(), 2, "wChannels");

        // A short buffer is filled only as far as it goes.
        let short = STACK_BASE + 0x3100;
        memory.write_bytes(short, &[0xbb; 8]).unwrap();
        assert_eq!(
            call_shim(&mut host, &mut memory, "waveOutGetDevCapsA", &[0, short, 6]).unwrap(),
            Return(0)
        );
        assert_eq!(memory.read_bytes(short + 6, 2).unwrap(), [0xbb; 2], "untouched");
    }

    #[test]
    fn every_command_says_there_is_no_mci_device() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // MCI_OPEN, as the guest sends it for its CD audio.
        assert_eq!(
            call_shim(
                &mut host,
                &mut memory,
                "mciSendCommandA",
                &[0, 0x0803, 0x3000, STACK_BASE + 0xa100]
            )
            .unwrap(),
            Return(MCIERR_DEVICE_NOT_INSTALLED)
        );
        // Ten more commands add one notice, not ten.
        for _ in 0..10 {
            call_shim(&mut host, &mut memory, "mciSendCommandA", &[0, 0x0803, 0, 0]).unwrap();
        }
        assert_eq!(host.notices.len(), 1);
    }

    #[test]
    fn the_timer_resolution_is_a_millisecond() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let caps = STACK_BASE + 0xa200;
        assert_eq!(
            call_shim(&mut host, &mut memory, "timeGetDevCaps", &[caps, 8]).unwrap(),
            Return(0)
        );
        assert_eq!(memory.read_u32(caps).unwrap(), 1);
        assert_eq!(memory.read_u32(caps + 4).unwrap(), 1000);
        assert_eq!(
            call_shim(&mut host, &mut memory, "timeBeginPeriod", &[1]).unwrap(),
            Return(0)
        );
    }

    #[test]
    fn a_timer_is_handed_out_scheduled_and_killed() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let callback = 0x0040_1000;
        let id = match call_shim(
            &mut host,
            &mut memory,
            "timeSetEvent",
            &[33, 33, callback, 0x1234, TIME_PERIODIC],
        )
        .unwrap()
        {
            Return(id) => id,
            other => panic!("timeSetEvent returned {other:?}"),
        };
        assert_ne!(id, 0, "a timer needs an id the guest can kill");
        assert_eq!(host.timers.count(), 1);

        // Nothing fires before the interval passes, and the first look at a timer
        // is what schedules it, because a shim does not know the clock.
        assert_eq!(host.timers.due(0), None);
        let interval = 33;
        assert_eq!(
            host.timers.due(interval),
            Some((id, callback, 0x1234)),
            "the callback and its dwUser come back unchanged"
        );
        // A periodic timer keeps firing.
        assert!(host.timers.due(interval * 2).is_some());

        assert_eq!(
            call_shim(&mut host, &mut memory, "timeKillEvent", &[id]).unwrap(),
            Return(0)
        );
        assert_eq!(host.timers.count(), 0);
        assert_eq!(host.timers.due(interval * 100), None);
        // One notice per timer, not one per call.
        assert_eq!(host.notices.len(), 1, "{:?}", host.notices);
        assert!(host.notices[0].contains("33 ms periodic"), "{}", host.notices[0]);
    }

    #[test]
    fn a_supplied_clock_decides_when_a_timer_is_due() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let callback = 0x0040_1000;
        call_shim(
            &mut host,
            &mut memory,
            "timeSetEvent",
            &[33, 33, callback, 0, TIME_PERIODIC],
        )
        .unwrap();

        // The page sets its own clock before each chunk. From here the timer's
        // schedule follows that, not the instruction count: a hundred million
        // instructions still take no guest time if the clock has not moved.
        host.set_clock(1_000);
        assert_eq!(
            host.guest_time_ms(100_000_000),
            1_000,
            "the supplied clock is the guest's time"
        );
        assert_eq!(host.timers.due(host.guest_time_ms(100_000_000)), None);
        host.set_clock(1_033);
        assert_eq!(
            host.timers.due(host.guest_time_ms(100_000_000)),
            Some((1, callback, 0)),
            "33 ms of the page's time is what makes a 33 ms timer due"
        );

        // Without a clock, the instruction count is the fallback, which is what
        // keeps a headless run repeatable.
        let headless = Host::new();
        assert_eq!(
            headless.guest_time_ms(INSTRUCTIONS_PER_MILLISECOND * 7),
            7,
            "no clock: 10,000 instructions is a millisecond"
        );
    }

    #[test]
    fn a_one_shot_timer_fires_once() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert!(matches!(
            call_shim(&mut host, &mut memory, "timeSetEvent", &[1, 1, 0x0040_1000, 0, 0]).unwrap(),
            Return(id) if id != 0
        ));
        assert_eq!(host.timers.due(0), None);
        let interval = 1;
        assert!(host.timers.due(interval).is_some());
        assert_eq!(host.timers.count(), 0, "a one-shot timer is spent");
        assert_eq!(host.timers.due(interval * 2), None);
    }

    #[test]
    fn the_devices_that_are_missing_say_so() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        assert_eq!(
            call_shim(&mut host, &mut memory, "auxGetNumDevs", &[]).unwrap(),
            Return(0)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "auxGetVolume", &[0, 0]).unwrap(),
            Return(MMSYSERR_NODRIVER)
        );
        assert_eq!(
            call_shim(&mut host, &mut memory, "joyGetPosEx", &[0, 0]).unwrap(),
            Return(JOYERR_UNPLUGGED)
        );
    }
}
