//! `MSS32.dll`: the Miles Sound System, which is the only way GTA Vice City makes
//! a sound.
//!
//! Miles is a C API of opaque handles -- `HDIGDRIVER`, `HSAMPLE`, `HSTREAM` --
//! and the game treats every one of them as "non-zero or the sound does not
//! exist". That shapes this shim: a handle is a number this host remembers, and
//! the calls around it are bookkeeping over guest memory the game has already
//! filled with PCM.
//!
//! # Where the sound comes from
//!
//! The game loads a `.wav`, hands the sample data to `AIL_mem_alloc_lock` or its
//! own heap, points a sample at it with `AIL_set_sample_address`, and starts it.
//! All of that is guest memory by the time the guest asks for a sound, so this
//! module does not decode anything: it publishes a voice to the same mixer
//! DirectSound feeds ([`crate::shim::dsound`]), live from the guest's own bytes,
//! and the page mixes it. `AIL_sample_status` then reports
//! [`SMP_PLAYING`] until the sample has run out, which is the one status the
//! shipped `gta-vc.exe` compares against: every call site is
//! `cmp eax, 4`.
//!
//! # What is recorded rather than rendered
//!
//! - **3D samples.** Miles' positional provider is answered (`M3D_NOERR`, one
//!   enumerated provider) and every 3D call is recorded, but the page's mixer is
//!   stereo: a 3D sample is mixed like a 2D one. `AIL_set_3D_position` and the
//!   distance pair are kept so a mixer that understands them can be added
//!   without the guest changing.
//! - **Loop blocks.** `AIL_set_sample_loop_block` is what an engine loop is, and
//!   the page mixer loops the whole sample, so the block is stored and the loop
//!   covers the file.
//! - **Streams.** Vice City's radio is MP3 (and `.adf`) played through
//!   `AIL_open_stream`. This host opens the file, tracks position, length,
//!   volume, pan and loop count exactly, and publishes no samples: it has no
//!   MP3 decoder. A one-line notice says so the first time a stream starts, so a
//!   silent radio is a reported limitation rather than a mystery.
//!
//! # Volumes and pans
//!
//! Miles uses `0..127` for both, with `64` the centre of a pan. The mixer speaks
//! DirectSound's hundredths of a decibel (`-10000..0`) and `-10000..10000`, so
//! every value crosses that boundary here, once.

use std::collections::HashMap;

use crate::shim::dsound::{AudioEvent, EVENT_PLAY, EVENT_STOP, EVENT_UPDATE};
use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const MSS32: &str = "mss32.dll";

/// `AIL_sample_status`/`AIL_stream_status` values.
///
/// Only [`SMP_PLAYING`] is load-bearing: `gta-vc.exe` compares the result of all
/// three status calls with `4` and nothing else (`cmp eax, 4` at `0x5d6406`,
/// `0x5d5aad` and `0x5d6432`). The rest are Miles' documented neighbours, and
/// anything that is not `4` already means "not playing".
pub const SMP_PLAYING: u32 = 4;
pub const SMP_DONE: u32 = 1;
pub const SMP_STOPPED: u32 = 3;

/// `M3D_NOERR`, which is what `AIL_open_3D_provider` returns on success.
const M3D_NOERR: u32 = 0;

/// `WAVE_FORMAT_PCM`, the only sample format `AIL_set_3D_sample_info` can mean
/// for a sample this host can mix.
const WAVE_FORMAT_PCM: u32 = 1;

/// Handles this module hands out. They are small, aligned, and above the ones
/// `user32` and `kernel32` use, so a log line reads at a glance.
const FIRST_HANDLE: u32 = 0x0002_0000;

/// Voice numbers given to the mixer, which are a namespace of their own: the
/// page keys its voices by this number and never confuses one with a DirectSound
/// object.
const FIRST_VOICE: u32 = 0x0004_0000;

/// The provider handle `AIL_enumerate_3D_providers` offers, which is the same
/// number whatever a guest passes to `AIL_open_3D_provider`.
const PROVIDER: u32 = 1;

/// A sample: 2D or 3D, which differ in what they store and not in how they play.
struct Sample {
    address: u32,
    length: u32,
    bits: u16,
    channels: u16,
    /// The rate the bytes were recorded at.
    rate: u32,
    /// `AIL_set_sample_playback_rate`, or zero for the recorded rate.
    playback_rate: u32,
    /// `0..127`, as Miles takes it.
    volume: i32,
    pan: i32,
    /// Zero plays once, a negative count loops for ever.
    loop_count: i32,
    loop_start: i32,
    loop_end: i32,
    playing: bool,
    /// Host clock when playback began, and the time already spent playing before
    /// the last pause. Together they give the position without a query.
    started_ms: u64,
    played_ms: u64,
    /// Whether the sample has finished a non-looping play, so `AIL_end_sample`
    /// and a natural ending can report different statuses.
    finished: bool,
    /// Whether the page has been told this voice stopped, so the stop event is
    /// queued once.
    stopped: bool,
    voice: u32,
    /// 3D calls this host records rather than renders.
    position: [f32; 3],
    max_distance: f32,
    min_distance: f32,
    effects_level: f32,
}

impl Sample {
    fn new(voice: u32) -> Self {
        Self {
            address: 0,
            length: 0,
            bits: 16,
            channels: 1,
            rate: 22_050,
            playback_rate: 0,
            volume: 127,
            pan: 64,
            loop_count: 0,
            loop_start: 0,
            loop_end: 0,
            playing: false,
            started_ms: 0,
            played_ms: 0,
            finished: false,
            stopped: false,
            voice,
            position: [0.0; 3],
            max_distance: 10.0,
            min_distance: 1.0,
            effects_level: 0.0,
        }
    }

    fn frame_bytes(&self) -> u32 {
        u32::from(self.channels).max(1) * u32::from(self.bits / 8).max(1)
    }

    fn frames(&self) -> u32 {
        self.length / self.frame_bytes()
    }

    fn play_rate(&self) -> u32 {
        if self.playback_rate == 0 {
            self.rate
        } else {
            self.playback_rate
        }
    }

    /// How long one pass over the sample takes.
    fn duration_ms(&self) -> u64 {
        let rate = u64::from(self.play_rate().max(1));
        if self.length == 0 || self.bits == 0 {
            return 0;
        }
        u64::from(self.frames()).saturating_mul(1000) / rate
    }

    /// Milliseconds of sound this sample owes, counting loops.
    fn total_ms(&self) -> Option<u64> {
        let once = self.duration_ms();
        if once == 0 {
            return Some(0);
        }
        match self.loop_count {
            0 => Some(once),
            count if count < 0 => None,
            count => Some(once.saturating_mul(count as u64)),
        }
    }

    fn elapsed_ms(&self, now: u64) -> u64 {
        if !self.playing {
            return self.played_ms;
        }
        self.played_ms
            .saturating_add(now.saturating_sub(self.started_ms))
    }

    /// What `AIL_sample_status` answers, and whether the answer just became
    /// final (which is when the host queues the stop).
    fn status(&self, now: u64) -> u32 {
        if self.finished {
            return SMP_DONE;
        }
        if !self.playing {
            return SMP_STOPPED;
        }
        match self.total_ms() {
            Some(total) if self.elapsed_ms(now) >= total => SMP_DONE,
            _ => SMP_PLAYING,
        }
    }

    /// The event that starts this voice.
    fn event(&self) -> AudioEvent {
        AudioEvent {
            kind: EVENT_PLAY,
            voice: self.voice,
            channels: u32::from(self.channels),
            rate: self.rate,
            frames: self.frames(),
            volume: volume_of(self.volume),
            pan: pan_of(self.pan),
            flags: u32::from(self.loop_count != 0),
            bytes: self.length,
            frequency: self.play_rate(),
            bits: u32::from(self.bits),
        }
    }

    /// The event that changes a voice already on the mixer.
    fn update(&self) -> AudioEvent {
        AudioEvent {
            kind: EVENT_UPDATE,
            volume: volume_of(self.volume),
            pan: pan_of(self.pan),
            frequency: self.play_rate(),
            ..self.event()
        }
    }
}

/// A stream: a file Miles reads in the background, which for Vice City means a
/// radio station.
struct Stream {
    name: String,
    /// The file handle `host.files` gave this stream, so its bytes are read on
    /// demand instead of copied.
    handle: u32,
    length: u32,
    /// The whole track in milliseconds when the container says (a WAV's data
    /// chunk, or an MP3's frames), otherwise zero.
    total_ms: u64,
    played_ms: u64,
    started_ms: u64,
    /// Where `AIL_set_stream_ms_position` last put the cursor.
    offset_ms: u64,
    playing: bool,
    paused: bool,
    loop_count: i32,
    volume: i32,
    pan: i32,
    /// Whether the page has been told this stream cannot be mixed, so the notice
    /// is written once per stream.
    reported: bool,
}

impl Stream {
    fn position_ms(&self, now: u64) -> u64 {
        if !self.playing || self.paused {
            return self.played_ms.saturating_add(self.offset_ms);
        }
        self.offset_ms
            .saturating_add(self.played_ms)
            .saturating_add(now.saturating_sub(self.started_ms))
    }

    fn status(&self, now: u64) -> u32 {
        if !self.playing {
            return SMP_STOPPED;
        }
        if self.loop_count != 0 || self.total_ms == 0 {
            return SMP_PLAYING;
        }
        if self.position_ms(now) >= self.total_ms {
            SMP_DONE
        } else {
            SMP_PLAYING
        }
    }
}

/// Everything the Miles entry points remember between calls.
#[derive(Default)]
pub struct Mss {
    started: bool,
    last_error: String,
    /// The string `AIL_last_error` handed out last, so a second call rewrites
    /// the same buffer instead of leaking one per error.
    error_buffer: Option<(u32, u32)>,
    digital: Option<u32>,
    samples: HashMap<u32, Sample>,
    streams: HashMap<u32, Stream>,
    provider: Option<u32>,
    room_type: i32,
    speaker_type: i32,
    preferences: HashMap<u32, i32>,
    /// `AIL_set_file_callbacks`, recorded because the guest installed them: this
    /// host reads the payload table when a name arrives, which is the same bytes
    /// for installed data.
    callbacks: Option<[u32; 4]>,
    next_handle: u32,
    next_voice: u32,
    /// How many samples and streams the guest started, which is what a trace and
    /// a test read.
    plays: u64,
}

impl Mss {
    fn handle(&mut self) -> u32 {
        self.next_handle += 1;
        FIRST_HANDLE + self.next_handle * 4
    }

    fn voice(&mut self) -> u32 {
        let voice = FIRST_VOICE + self.next_voice * 4;
        self.next_voice += 1;
        voice
    }

    /// The samples the guest started, which a test reads to tell "the game asked
    /// for a sound" from "the game was silent".
    pub fn plays(&self) -> u64 {
        self.plays
    }

    pub fn sample_count(&self) -> usize {
        self.samples.len()
    }

    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Whether `AIL_open_digital_driver` has a device.
    pub fn is_open(&self) -> bool {
        self.digital.is_some()
    }

    fn error(&mut self, text: &str) {
        self.last_error = text.to_string();
    }
}

// ---- volume and pan -------------------------------------------------------

/// Miles' `0..127` level as DirectSound's `-10000..0`.
///
/// Both scales are logarithmic in intent: Miles' scale is linear in amplitude
/// steps and DirectSound's is hundredths of a decibel, so the mapping here is the
/// one DirectSound's own `DSBVOLUME_MIN` implies -- full scale at the top and
/// silence at zero.
fn volume_of(level: i32) -> i32 {
    let level = level.clamp(0, 127);
    if level == 127 {
        return 0;
    }
    -((127 - level) * 10_000 / 127)
}

/// Miles' `0..127` pan, `64` centred, as `-10000..10000`.
fn pan_of(pan: i32) -> i32 {
    let pan = pan.clamp(0, 127);
    ((pan - 64) * 10_000 / if pan < 64 { 64 } else { 63 }).clamp(-10_000, 10_000)
}

pub const SHIMS: &[Binding] = &[
    // ---- the library and its one device --------------------------------
    Binding { dll: MSS32, name: "AIL_startup", arg_bytes: 0, shim: startup },
    Binding { dll: MSS32, name: "AIL_shutdown", arg_bytes: 0, shim: shutdown },
    Binding { dll: MSS32, name: "AIL_set_preference", arg_bytes: 8, shim: set_preference },
    Binding { dll: MSS32, name: "AIL_last_error", arg_bytes: 0, shim: last_error },
    Binding { dll: MSS32, name: "AIL_open_digital_driver", arg_bytes: 16, shim: open_digital_driver },
    Binding { dll: MSS32, name: "AIL_close_digital_driver", arg_bytes: 4, shim: close_digital_driver },
    Binding { dll: MSS32, name: "AIL_digital_handle_release", arg_bytes: 4, shim: digital_handle },
    Binding { dll: MSS32, name: "AIL_digital_handle_reacquire", arg_bytes: 4, shim: digital_handle },
    Binding { dll: MSS32, name: "AIL_set_redist_directory", arg_bytes: 4, shim: redist_directory },
    Binding { dll: MSS32, name: "AIL_set_file_callbacks", arg_bytes: 16, shim: set_file_callbacks },

    // ---- memory the library lends the guest ----------------------------
    Binding { dll: MSS32, name: "AIL_mem_alloc_lock", arg_bytes: 4, shim: mem_alloc_lock },
    Binding { dll: MSS32, name: "AIL_mem_free_lock", arg_bytes: 4, shim: mem_free_lock },

    // ---- samples -------------------------------------------------------
    Binding { dll: MSS32, name: "AIL_allocate_sample_handle", arg_bytes: 4, shim: allocate_sample },
    Binding { dll: MSS32, name: "AIL_init_sample", arg_bytes: 4, shim: init_sample },
    Binding { dll: MSS32, name: "AIL_release_sample_handle", arg_bytes: 4, shim: release_sample },
    Binding { dll: MSS32, name: "AIL_set_sample_address", arg_bytes: 12, shim: set_sample_address },
    Binding { dll: MSS32, name: "AIL_set_sample_type", arg_bytes: 12, shim: set_sample_type },
    Binding { dll: MSS32, name: "AIL_set_sample_playback_rate", arg_bytes: 8, shim: set_sample_rate },
    Binding { dll: MSS32, name: "AIL_set_sample_volume", arg_bytes: 8, shim: set_sample_volume },
    Binding { dll: MSS32, name: "AIL_set_sample_pan", arg_bytes: 8, shim: set_sample_pan },
    Binding { dll: MSS32, name: "AIL_set_sample_loop_count", arg_bytes: 8, shim: set_sample_loop_count },
    Binding { dll: MSS32, name: "AIL_set_sample_loop_block", arg_bytes: 12, shim: set_sample_loop_block },
    Binding { dll: MSS32, name: "AIL_sample_status", arg_bytes: 4, shim: sample_status },
    Binding { dll: MSS32, name: "AIL_start_sample", arg_bytes: 4, shim: start_sample },
    Binding { dll: MSS32, name: "AIL_end_sample", arg_bytes: 4, shim: end_sample },

    // ---- 3D samples, answered and recorded -----------------------------
    Binding { dll: MSS32, name: "AIL_open_3D_provider", arg_bytes: 4, shim: open_3d_provider },
    Binding { dll: MSS32, name: "AIL_close_3D_provider", arg_bytes: 4, shim: close_3d_provider },
    Binding { dll: MSS32, name: "AIL_enumerate_3D_providers", arg_bytes: 12, shim: enumerate_3d_providers },
    Binding { dll: MSS32, name: "AIL_3D_provider_attribute", arg_bytes: 12, shim: provider_attribute },
    Binding { dll: MSS32, name: "AIL_set_3D_provider_preference", arg_bytes: 12, shim: provider_attribute },
    Binding { dll: MSS32, name: "AIL_3D_room_type", arg_bytes: 4, shim: room_type },
    Binding { dll: MSS32, name: "AIL_set_3D_room_type", arg_bytes: 8, shim: set_room_type },
    Binding { dll: MSS32, name: "AIL_set_3D_speaker_type", arg_bytes: 8, shim: set_speaker_type },
    Binding { dll: MSS32, name: "AIL_allocate_3D_sample_handle", arg_bytes: 4, shim: allocate_sample },
    Binding { dll: MSS32, name: "AIL_release_3D_sample_handle", arg_bytes: 4, shim: release_sample },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_info", arg_bytes: 8, shim: set_3d_sample_info },
    Binding { dll: MSS32, name: "AIL_3D_sample_status", arg_bytes: 4, shim: sample_status },
    Binding { dll: MSS32, name: "AIL_start_3D_sample", arg_bytes: 4, shim: start_sample },
    Binding { dll: MSS32, name: "AIL_end_3D_sample", arg_bytes: 4, shim: end_sample },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_volume", arg_bytes: 8, shim: set_sample_volume },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_playback_rate", arg_bytes: 8, shim: set_sample_rate },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_loop_count", arg_bytes: 8, shim: set_sample_loop_count },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_loop_block", arg_bytes: 12, shim: set_sample_loop_block },
    Binding { dll: MSS32, name: "AIL_set_3D_position", arg_bytes: 16, shim: set_3d_position },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_distances", arg_bytes: 12, shim: set_3d_distances },
    Binding { dll: MSS32, name: "AIL_set_3D_sample_effects_level", arg_bytes: 8, shim: set_3d_effects },

    // ---- streams, tracked rather than decoded --------------------------
    Binding { dll: MSS32, name: "AIL_open_stream", arg_bytes: 12, shim: open_stream },
    Binding { dll: MSS32, name: "AIL_close_stream", arg_bytes: 4, shim: close_stream },
    Binding { dll: MSS32, name: "AIL_start_stream", arg_bytes: 4, shim: start_stream },
    Binding { dll: MSS32, name: "AIL_pause_stream", arg_bytes: 8, shim: pause_stream },
    Binding { dll: MSS32, name: "AIL_service_stream", arg_bytes: 8, shim: service_stream },
    Binding { dll: MSS32, name: "AIL_stream_status", arg_bytes: 4, shim: stream_status },
    Binding { dll: MSS32, name: "AIL_set_stream_volume", arg_bytes: 8, shim: set_stream_volume },
    Binding { dll: MSS32, name: "AIL_set_stream_pan", arg_bytes: 8, shim: set_stream_pan },
    Binding { dll: MSS32, name: "AIL_set_stream_loop_count", arg_bytes: 8, shim: set_stream_loop_count },
    Binding { dll: MSS32, name: "AIL_set_stream_ms_position", arg_bytes: 8, shim: set_stream_position },
    Binding { dll: MSS32, name: "AIL_stream_ms_position", arg_bytes: 12, shim: stream_position },
];

// ---- the library ----------------------------------------------------------

fn startup(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    call.arg(0).ok();
    Ok(Return(1))
}

fn shutdown(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    call.arg(0).ok();
    stop_everything(host);
    host.mss.started = false;
    host.mss.samples.clear();
    host.mss.digital = None;
    host.mss.provider = None;
    Ok(Return(0))
}

/// Stops every sample and closes every stream, which is what the driver being
/// closed means: the game's next call reopens it.
fn stop_everything(host: &mut Host) {
    let now = host.now_ms();
    let voices: Vec<(u32, AudioEvent)> = host
        .mss
        .samples
        .values()
        .filter(|sample| sample.playing && !sample.stopped)
        .map(|sample| {
            (
                sample.voice,
                AudioEvent {
                    kind: EVENT_STOP,
                    voice: sample.voice,
                    ..sample.event()
                },
            )
        })
        .collect();
    for (voice, event) in voices {
        host.audio.notify(event);
        host.audio.silence(voice);
    }
    for sample in host.mss.samples.values_mut() {
        sample.played_ms = sample.elapsed_ms(now);
        sample.playing = false;
        sample.stopped = true;
    }
    let handles: Vec<u32> = host.mss.streams.values().map(|stream| stream.handle).collect();
    for handle in handles {
        host.files.close(handle);
    }
    host.mss.streams.clear();
}

/// `AIL_startup()`: nothing to open, and a non-zero answer is what makes the
/// game continue.
fn set_preference(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let number = call.arg(0)?;
    let value = call.arg(1)? as i32;
    host.mss.preferences.insert(number, value);
    Ok(Return(1))
}

/// `AIL_last_error()`: a C string the guest may print.
fn last_error(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = if host.mss.last_error.is_empty() {
        "no error".to_string()
    } else {
        host.mss.last_error.clone()
    };
    let bytes = text.as_bytes();
    let needed = bytes.len() as u32 + 1;
    let address = match host.mss.error_buffer {
        Some((address, capacity)) if capacity >= needed => address,
        Some((address, _)) => {
            host.release(address);
            host.allocate(needed, 4)?
        }
        None => host.allocate(needed, 4)?,
    };
    host.mss.error_buffer = Some((address, needed));
    call.write(address, bytes)?;
    call.write(address + bytes.len() as u32, &[0])?;
    Ok(Return(address))
}

/// `AIL_open_digital_driver(frequency, bits, channels, flags)`.
///
/// The requested format is recorded and the handle is real, but the mixer is the
/// page's: it takes samples at their own rate in 8 or 16 bits, so nothing about
/// the driver constrains what can be played. A null handle would make the game
/// print Miles' error and give up on sound, so this always succeeds.
fn open_digital_driver(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let frequency = call.arg(0)?;
    let bits = call.arg(1)? as i32;
    let channels = call.arg(2)? as i32;
    call.arg(3)?;
    let handle = host.mss.handle();
    host.mss.digital = Some(handle);
    host.mss.started = true;
    host.notice(format!(
        "Miles: digital driver {frequency} Hz, {bits}-bit, {channels} channel(s)"
    ));
    Ok(Return(handle))
}

fn close_digital_driver(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    call.arg(0)?;
    stop_everything(host);
    host.mss.digital = None;
    Ok(Return(0))
}

fn digital_handle(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn redist_directory(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    // Miles answers with the directory it now uses; this host has no separate
    // redist directory, so the argument is the answer.
    Ok(Return(call.arg(0)?))
}

/// `AIL_set_file_callbacks(open, close, seek, read)`.
///
/// Vice City installs its own file manager here so Miles reads through the
/// game's virtual file system. This host reads the payload table by name
/// instead, which is the same bytes for an installed copy, and records the
/// callbacks so a trace can show the guest asked for them.
fn set_file_callbacks(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let open = call.arg(0)?;
    let close = call.arg(1)?;
    let seek = call.arg(2)?;
    let read = call.arg(3)?;
    let first = host.mss.callbacks.is_none();
    host.mss.callbacks = Some([open, close, seek, read]);
    if first {
        host.notice(
            "Miles: file callbacks installed; streams are opened from the payload by name".to_string(),
        );
    }
    Ok(Return(0))
}

// ---- memory ----------------------------------------------------------------

/// `AIL_mem_alloc_lock(size)`: memory the guest may write its PCM into.
fn mem_alloc_lock(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let size = call.arg(0)?;
    match host.allocate(size.max(1), 4) {
        Ok(pointer) => Ok(Return(pointer)),
        Err(error) => {
            host.mss.error(&format!("AIL_mem_alloc_lock({size}): {error}"));
            Ok(Return(0))
        }
    }
}

fn mem_free_lock(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.release(call.arg(0)?);
    Ok(Return(0))
}

// ---- samples ---------------------------------------------------------------

fn allocate_sample(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    call.arg(0)?;
    let voice = host.mss.voice();
    let handle = host.mss.handle();
    host.mss.samples.insert(handle, Sample::new(voice));
    Ok(Return(handle))
}

fn release_sample(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    if let Some(sample) = host.mss.samples.remove(&handle) {
        host.audio.notify(AudioEvent {
            kind: EVENT_STOP,
            voice: sample.voice,
            ..sample.event()
        });
        host.audio.silence(sample.voice);
    }
    Ok(Return(0))
}

fn init_sample(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let voice = host.mss.samples.get(&handle).map_or(0, |sample| sample.voice);
    let voice = if voice == 0 { host.mss.voice() } else { voice };
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        host.audio.silence(sample.voice);
        *sample = Sample::new(voice);
    }
    Ok(Return(0))
}

fn set_sample_address(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let address = call.arg(1)?;
    let length = call.arg(2)?;
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.address = address;
        sample.length = length;
    }
    Ok(Return(0))
}

/// `AIL_set_sample_type(sample, format, flags)`.
///
/// The shipped `gta-vc.exe` calls this with `(1, 1)` for a 16-bit mono sample --
/// `0x5d7a6c` pushes the pair before the handle -- so `1` is the value that has
/// to mean "16-bit mono". Miles' `DIG_F_*` bit patterns are accepted too, because
/// a guest built against a newer header spells the same format `0x10` or `0x18`,
/// and a format this host cannot mix leaves the sample silent rather than
/// pretending.
fn set_sample_type(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let format = call.arg(1)?;
    call.arg(2)?;
    let Some((channels, bits)) = decode_format(format) else {
        host.mss.error(&format!("sample format {format:#x} is compressed; this host mixes PCM only"));
        if let Some(sample) = host.mss.samples.get_mut(&handle) {
            sample.bits = 0;
            sample.length = 0;
        }
        return Ok(Return(0));
    };
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.channels = channels;
        sample.bits = bits;
    }
    Ok(Return(0))
}

/// `(channels, bits)` for a `DIG_F_*` format, or `None` for a compressed one.
fn decode_format(format: u32) -> Option<(u16, u16)> {
    match format {
        // Verified from the shipped executable's own call site: it passes 1.
        1 => Some((1, 16)),
        0x08 => Some((1, 8)),
        0x10 => Some((1, 16)),
        0x18 => Some((2, 8)),
        0x20 => Some((2, 16)),
        _ => None,
    }
}

fn set_sample_rate(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let rate = call.arg(1)?;
    let mut update = None;
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.playback_rate = rate;
        if sample.playing && !sample.stopped {
            update = Some(sample.update());
        }
    }
    if let Some(event) = update {
        host.audio.notify(event);
    }
    Ok(Return(0))
}

fn set_sample_volume(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let level = call.arg(1)? as i32;
    let mut update = None;
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.volume = level.clamp(0, 127);
        if sample.playing && !sample.stopped {
            update = Some(sample.update());
        }
    }
    if let Some(event) = update {
        host.audio.notify(event);
    }
    Ok(Return(0))
}

fn set_sample_pan(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let pan = call.arg(1)? as i32;
    let mut update = None;
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.pan = pan.clamp(0, 127);
        if sample.playing && !sample.stopped {
            update = Some(sample.update());
        }
    }
    if let Some(event) = update {
        host.audio.notify(event);
    }
    Ok(Return(0))
}

fn set_sample_loop_count(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let count = call.arg(1)? as i32;
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.loop_count = count;
    }
    Ok(Return(0))
}

fn set_sample_loop_block(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let start = call.arg(1)? as i32;
    let end = call.arg(2)? as i32;
    let first = host.mss.samples.get(&handle).is_some_and(|sample| {
        sample.loop_start == 0 && sample.loop_end == 0
    });
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.loop_start = start;
        sample.loop_end = end;
    }
    if first {
        host.notice(
            "Miles: loop blocks are recorded, but the page mixer loops the whole sample".to_string(),
        );
    }
    Ok(Return(0))
}

fn sample_status(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let now = host.now_ms();
    let Some(sample) = host.mss.samples.get(&handle) else {
        return Ok(Return(SMP_STOPPED));
    };
    let status = sample.status(now);
    if status == SMP_DONE && sample.playing {
        // The sound has run out: the mixer is told once, and the sample keeps
        // answering `SMP_DONE` for the rest of its life, the way Miles does.
        let event = AudioEvent {
            kind: EVENT_STOP,
            voice: sample.voice,
            ..sample.event()
        };
        let voice = sample.voice;
        if let Some(sample) = host.mss.samples.get_mut(&handle) {
            sample.played_ms = sample.elapsed_ms(now);
            sample.playing = false;
            sample.finished = true;
            sample.stopped = true;
        }
        host.audio.notify(event);
        host.audio.silence(voice);
    }
    Ok(Return(status))
}

fn start_sample(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let now = host.now_ms();
    let Some(sample) = host.mss.samples.get_mut(&handle) else {
        return Ok(Return(0));
    };
    if sample.address == 0 || sample.length == 0 || sample.bits == 0 {
        // Nothing to play: Miles also starts nothing, and the status stays
        // stopped, which the game reads as "this sound is not happening".
        return Ok(Return(0));
    }
    sample.playing = true;
    sample.finished = false;
    sample.stopped = false;
    sample.started_ms = now;
    sample.played_ms = 0;
    let event = sample.event();
    let (address, length) = (sample.address, sample.length);
    host.mss.plays += 1;
    host.audio.play_live(event, address, length);
    Ok(Return(0))
}

fn end_sample(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let now = host.now_ms();
    let Some(sample) = host.mss.samples.get_mut(&handle) else {
        return Ok(Return(0));
    };
    if sample.playing || !sample.stopped {
        let event = AudioEvent {
            kind: EVENT_STOP,
            voice: sample.voice,
            ..sample.event()
        };
        sample.played_ms = sample.elapsed_ms(now);
        sample.playing = false;
        sample.stopped = true;
        let voice = sample.voice;
        host.audio.notify(event);
        host.audio.silence(voice);
    }
    Ok(Return(0))
}

// ---- 3D --------------------------------------------------------------------

fn open_3d_provider(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let provider = call.arg(0)?;
    host.mss.provider = Some(if provider == 0 { PROVIDER } else { provider });
    host.notice(
        "Miles: 3D provider open; positional samples are mixed as stereo".to_string(),
    );
    Ok(Return(M3D_NOERR))
}

fn close_3d_provider(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    call.arg(0)?;
    host.mss.provider = None;
    Ok(Return(0))
}

/// `AIL_enumerate_3D_providers(&next, &provider, &name)`: one provider, then the
/// end of the list.
fn enumerate_3d_providers(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let next = call.arg(0)?;
    let provider = call.arg(1)?;
    let name = call.arg(2)?;
    let cursor = if next == 0 { 0 } else { call.memory.read_u32(next)? };
    if cursor != 0 {
        return Ok(Return(0));
    }
    if provider != 0 {
        call.write_u32(provider, PROVIDER)?;
    }
    if name != 0 {
        let bytes = b"Miles Fast 2D Positional Audio\0";
        let at = host.allocate(bytes.len() as u32, 4)?;
        call.write(at, bytes)?;
        call.write_u32(name, at)?;
    }
    if next != 0 {
        call.write_u32(next, 1)?;
    }
    Ok(Return(1))
}

/// `AIL_3D_provider_attribute` and `AIL_set_3D_provider_preference`: this host
/// has no provider attributes to report, so a query answers zero.
fn provider_attribute(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let value = call.arg(2)?;
    if value != 0 {
        call.write_u32(value, 0)?;
    }
    Ok(Return(0))
}

fn room_type(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(host.mss.room_type as u32))
}

fn set_room_type(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.mss.room_type = call.arg(1)? as i32;
    Ok(Return(0))
}

fn set_speaker_type(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    host.mss.speaker_type = call.arg(1)? as i32;
    Ok(Return(0))
}

/// `AIL_set_3D_sample_info(sample, &AILSOUNDINFO)`: the 3D half's way of giving
/// a sample its bytes and format in one call.
fn set_3d_sample_info(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let info = call.arg(1)?;
    if info == 0 {
        return Ok(Return(0));
    }
    let format = call.memory.read_u32(info)?;
    let address = call.memory.read_u32(info + 4)?;
    let length = call.memory.read_u32(info + 8)?;
    let rate = call.memory.read_u32(info + 12)?;
    let bits = call.memory.read_u32(info + 16)?;
    let channels = call.memory.read_u32(info + 20)?;
    if format != WAVE_FORMAT_PCM {
        host.mss
            .error(&format!("3D sample format {format} is not PCM; this host mixes PCM only"));
        return Ok(Return(0));
    }
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.address = address;
        sample.length = length;
        sample.rate = rate.max(1);
        sample.bits = bits.clamp(1, 16) as u16;
        sample.channels = channels.clamp(1, 2) as u16;
    }
    Ok(Return(1))
}

fn set_3d_position(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let x = f32::from_bits(call.arg(1)?);
    let y = f32::from_bits(call.arg(2)?);
    let z = f32::from_bits(call.arg(3)?);
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.position = [x, y, z];
    }
    Ok(Return(0))
}

fn set_3d_distances(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let max = f32::from_bits(call.arg(1)?);
    let min = f32::from_bits(call.arg(2)?);
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.max_distance = max;
        sample.min_distance = min;
    }
    Ok(Return(0))
}

fn set_3d_effects(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let level = f32::from_bits(call.arg(1)?);
    if let Some(sample) = host.mss.samples.get_mut(&handle) {
        sample.effects_level = level;
    }
    Ok(Return(0))
}

// ---- streams ---------------------------------------------------------------

/// `AIL_open_stream(driver, filename, mem)`.
///
/// A zero answer is what tells the game "this file is not there", so a name the
/// payload does not have fails here rather than handing back a stream that will
/// never play. The length comes from the container when it can be read: a WAV's
/// data chunk or an MP3's frames, which is what `AIL_stream_ms_position` reports
/// and what the radio's track list is built from.
fn open_stream(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(1)?;
    call.arg(2)?;
    if name == 0 {
        host.mss.error("AIL_open_stream: no filename");
        return Ok(Return(0));
    }
    let path = call.memory.read_cstr(name)?;
    let Some(handle) = host.files.open(&path) else {
        host.mss.error(&format!("AIL_open_stream: {path:?} is not in the payload"));
        return Ok(Return(0));
    };
    let length = host.files.length(handle).unwrap_or(0);
    let total_ms = host
        .files
        .contents(&path)
        .map_or(0, |bytes| stream_duration_ms(bytes));
    let stream = Stream {
        name: path.clone(),
        handle,
        length,
        total_ms,
        played_ms: 0,
        started_ms: host.now_ms(),
        offset_ms: 0,
        playing: false,
        paused: false,
        loop_count: 0,
        volume: 127,
        pan: 64,
        reported: false,
    };
    let stream_handle = host.mss.handle();
    host.mss.streams.insert(stream_handle, stream);
    host.notice(format!(
        "Miles: stream {path:?} ({length} bytes, {} ms)",
        host.mss
            .streams
            .get(&stream_handle)
            .map_or(0, |stream| stream.total_ms)
    ));
    Ok(Return(stream_handle))
}

/// The length of a stream, in milliseconds, from the container alone.
///
/// A WAV says so outright; an MP3 needs its frames walked, which is what the
/// radio stations are and worth the forty lines: the track list the game shows
/// is built from the total this answers.
fn stream_duration_ms(bytes: &[u8]) -> u64 {
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        return wav_duration_ms(bytes).unwrap_or(0);
    }
    if bytes.len() >= 4 && bytes[0] == 0xff && bytes[1] & 0xe0 == 0xe0 {
        return mp3_duration_ms(bytes).unwrap_or(0);
    }
    0
}

fn wav_duration_ms(bytes: &[u8]) -> Option<u64> {
    let mut at = 12_usize;
    let mut byte_rate = 0_u64;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().ok()?) as usize;
        let body = at + 8;
        match id {
            b"fmt " => {
                if body + 12 > bytes.len() {
                    return None;
                }
                byte_rate = u64::from(u32::from_le_bytes(
                    bytes[body + 8..body + 12].try_into().ok()?,
                ));
            }
            b"data" => {
                if byte_rate == 0 {
                    return None;
                }
                let available = bytes.len().saturating_sub(body).min(size);
                return Some(u64::from(available as u32) * 1000 / byte_rate);
            }
            _ => {}
        }
        at = body.checked_add(size)?.checked_add(size & 1)?;
    }
    None
}

/// MP3 frame walking: every frame is 1152 samples, so the file's duration is the
/// sum over its frames. A frame whose header is not understood ends the walk,
/// which is the best a header-only parser can do.
fn mp3_duration_ms(bytes: &[u8]) -> Option<u64> {
    const BITRATES: [[u32; 15]; 2] = [
        // MPEG 1, layer III.
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320],
        // MPEG 2/2.5, layer III.
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160],
    ];
    const RATES: [u32; 4] = [44_100, 48_000, 32_000, 0];
    let mut at = 0_usize;
    let mut samples = 0_u64;
    let mut rate = 0_u32;
    let mut frames = 0_u32;
    while at + 4 <= bytes.len() {
        let header = u32::from_be_bytes(bytes[at..at + 4].try_into().ok()?);
        if header & 0xffe0_0000 != 0xffe0_0000 {
            at += 1;
            continue;
        }
        let version = (header >> 19) & 0x3;
        let layer = (header >> 17) & 0x3;
        let bitrate_index = ((header >> 12) & 0xf) as usize;
        let rate_index = ((header >> 10) & 0x3) as usize;
        let padding = (header >> 9) & 0x1;
        if layer != 1 || bitrate_index == 0 || bitrate_index == 15 {
            at += 1;
            continue;
        }
        let table = if version == 3 { 0 } else { 1 };
        let bitrate = BITRATES[table][bitrate_index] * 1000;
        let mut frame_rate = RATES[rate_index];
        if version == 2 {
            frame_rate /= 2;
        } else if version == 0 {
            frame_rate /= 4;
        }
        if frame_rate == 0 {
            at += 1;
            continue;
        }
        let samples_per_frame = if version == 3 { 1152 } else { 576 };
        let frame_bytes = (samples_per_frame / 8 * bitrate / frame_rate + padding) as usize;
        if frame_bytes == 0 {
            at += 1;
            continue;
        }
        if rate == 0 {
            rate = frame_rate;
        }
        samples += u64::from(samples_per_frame);
        frames += 1;
        at += frame_bytes;
    }
    if frames == 0 || rate == 0 {
        return None;
    }
    Some(samples * 1000 / u64::from(rate))
}

fn close_stream(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    if let Some(stream) = host.mss.streams.remove(&handle) {
        host.files.close(stream.handle);
    }
    Ok(Return(0))
}

fn start_stream(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let now = host.now_ms();
    let Some(stream) = host.mss.streams.get_mut(&handle) else {
        return Ok(Return(0));
    };
    stream.playing = true;
    stream.paused = false;
    stream.started_ms = now;
    let name = stream.name.clone();
    let report = !stream.reported;
    stream.reported = true;
    host.mss.plays += 1;
    if report {
        host.notice(format!(
            "Miles: stream {name:?} is tracked, not decoded: this host has no MP3 decoder, so radio is silent"
        ));
    }
    Ok(Return(0))
}

fn pause_stream(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let pause = call.arg(1)? != 0;
    let now = host.now_ms();
    if let Some(stream) = host.mss.streams.get_mut(&handle) {
        if pause && !stream.paused && stream.playing {
            stream.played_ms = stream
                .played_ms
                .saturating_add(now.saturating_sub(stream.started_ms));
            stream.paused = true;
        } else if !pause && stream.paused {
            stream.started_ms = now;
            stream.paused = false;
        }
    }
    Ok(Return(0))
}

/// `AIL_service_stream(stream, fillup)`: nothing is decoded, so the buffer the
/// guest would refill is already as full as it will get.
fn service_stream(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

fn stream_status(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let now = host.now_ms();
    match host.mss.streams.get(&handle) {
        Some(stream) => Ok(Return(stream.status(now))),
        None => Ok(Return(SMP_STOPPED)),
    }
}

fn set_stream_volume(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let level = call.arg(1)? as i32;
    if let Some(stream) = host.mss.streams.get_mut(&handle) {
        stream.volume = level.clamp(0, 127);
    }
    Ok(Return(0))
}

fn set_stream_pan(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let pan = call.arg(1)? as i32;
    if let Some(stream) = host.mss.streams.get_mut(&handle) {
        stream.pan = pan.clamp(0, 127);
    }
    Ok(Return(0))
}

fn set_stream_loop_count(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let count = call.arg(1)? as i32;
    if let Some(stream) = host.mss.streams.get_mut(&handle) {
        stream.loop_count = count;
    }
    Ok(Return(0))
}

/// `AIL_set_stream_ms_position(stream, ms)`: moves the cursor, which for a WAV
/// also moves the file's own read pointer.
fn set_stream_position(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let milliseconds = call.arg(1)? as i32;
    let now = host.now_ms();
    let mut seek = None;
    if let Some(stream) = host.mss.streams.get_mut(&handle) {
        stream.offset_ms = milliseconds.max(0) as u64;
        stream.played_ms = 0;
        stream.started_ms = now;
        if stream.total_ms != 0 && stream.length != 0 {
            let fraction = stream.offset_ms.min(stream.total_ms) as u128;
            let offset =
                ((fraction * u128::from(stream.length)) / u128::from(stream.total_ms)) as u32;
            seek = Some((stream.handle, offset));
        }
    }
    if let Some((file, offset)) = seek {
        host.files.seek(file, offset);
    }
    Ok(Return(0))
}

/// `AIL_stream_ms_position(stream, &total, &current)`.
fn stream_position(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let handle = call.arg(0)?;
    let total = call.arg(1)?;
    let current = call.arg(2)?;
    let now = host.now_ms();
    let Some(stream) = host.mss.streams.get(&handle) else {
        return Ok(Return(0));
    };
    let position = stream.position_ms(now).min(u64::from(u32::MAX)) as u32;
    let length = stream.total_ms.min(u64::from(u32::MAX)) as u32;
    if total != 0 {
        call.write_u32(total, length)?;
    }
    if current != 0 {
        call.write_u32(current, position)?;
    }
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
        let at = STACK_BASE + 0x8000;
        let mut bytes = Vec::new();
        for argument in arguments {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(at, &bytes).unwrap();
        let mut call = Call {
            memory,
            args: at,
            arg_bytes: binding.arg_bytes,
        };
        (binding.shim)(host, &mut call)
    }

    fn number(action: Action) -> u32 {
        match action {
            Return(value) => value,
            other => panic!("expected a return value, got {other:?}"),
        }
    }

    /// Opens the device and returns its handle.
    fn driver(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        number(
            call_shim(host, memory, "AIL_open_digital_driver", &[22_050, 16, 1, 0]).unwrap(),
        )
    }

    /// A sample pointed at `length` bytes of 16-bit mono PCM at 22050 Hz.
    fn ready_sample(host: &mut Host, memory: &mut GuestMemory, driver: u32, length: u32) -> u32 {
        let sample = number(call_shim(host, memory, "AIL_allocate_sample_handle", &[driver]).unwrap());
        let data = host.allocate(length, 4).unwrap();
        call_shim(host, memory, "AIL_init_sample", &[sample]).unwrap();
        call_shim(host, memory, "AIL_set_sample_address", &[sample, data, length]).unwrap();
        call_shim(host, memory, "AIL_set_sample_type", &[sample, 1, 1]).unwrap();
        call_shim(host, memory, "AIL_set_sample_playback_rate", &[sample, 22_050]).unwrap();
        sample
    }

    #[test]
    fn the_driver_and_a_sample_get_handles() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let driver = driver(&mut host, &mut memory);
        assert_ne!(driver, 0);
        assert!(host.mss.is_open());
        let sample = number(
            call_shim(&mut host, &mut memory, "AIL_allocate_sample_handle", &[driver]).unwrap(),
        );
        assert_ne!(sample, 0);
        assert_ne!(sample, driver, "a sample is not the driver");
        assert_eq!(host.mss.sample_count(), 1);
    }

    #[test]
    fn a_sixteen_bit_mono_sample_reaches_the_mixer_live() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.set_clock(1_000);
        let driver = driver(&mut host, &mut memory);
        // One second of 16-bit mono at 22050 Hz.
        let sample = ready_sample(&mut host, &mut memory, driver, 44_100);
        call_shim(&mut host, &mut memory, "AIL_set_sample_volume", &[sample, 64]).unwrap();
        call_shim(&mut host, &mut memory, "AIL_set_sample_pan", &[sample, 32]).unwrap();
        call_shim(&mut host, &mut memory, "AIL_start_sample", &[sample]).unwrap();

        let events = host.audio.events();
        assert_eq!(events.len(), 1);
        let event = events[0];
        assert_eq!(event.kind, EVENT_PLAY);
        assert_eq!((event.channels, event.bits), (1, 16));
        assert_eq!(event.rate, 22_050);
        assert_eq!(event.frequency, 22_050);
        assert_eq!(event.frames, 22_050);
        assert_eq!(event.bytes, 44_100);
        assert_eq!(event.flags, 0, "a one-shot does not loop");
        // Miles' 64 of 127 is not silence, and its centre pan is centred.
        assert_eq!(event.volume, volume_of(64));
        assert_eq!(event.pan, pan_of(32));
        assert_eq!(host.mss.plays(), 1);
        // The mixer reads the guest's own bytes, so a rewrite is heard.
        let (address, length) = host.audio.samples(event.voice).expect("a live voice");
        assert_eq!(length, 44_100);
        assert!(address > 0);
    }

    #[test]
    fn status_is_playing_until_the_sample_runs_out() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.set_clock(1_000);
        let driver = driver(&mut host, &mut memory);
        // A tenth of a second.
        let sample = ready_sample(&mut host, &mut memory, driver, 4_410);
        call_shim(&mut host, &mut memory, "AIL_start_sample", &[sample]).unwrap();
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_sample_status", &[sample]).unwrap()),
            SMP_PLAYING
        );
        host.set_clock(1_150);
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_sample_status", &[sample]).unwrap()),
            SMP_DONE,
            "the status the game compares against 4 is no longer 4"
        );
        let events = host.audio.events();
        assert_eq!(events.last().unwrap().kind, EVENT_STOP, "the mixer was told");
    }

    #[test]
    fn an_endless_loop_never_reports_done() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.set_clock(1_000);
        let driver = driver(&mut host, &mut memory);
        let sample = ready_sample(&mut host, &mut memory, driver, 4_410);
        call_shim(&mut host, &mut memory, "AIL_set_sample_loop_count", &[sample, u32::MAX]).unwrap();
        call_shim(&mut host, &mut memory, "AIL_start_sample", &[sample]).unwrap();
        assert_eq!(host.audio.events()[0].flags, 1, "the mixer loops it");
        host.set_clock(60_000);
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_sample_status", &[sample]).unwrap()),
            SMP_PLAYING
        );
    }

    #[test]
    fn setting_the_address_before_starting_is_what_makes_a_sound() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let driver = driver(&mut host, &mut memory);
        let sample = number(
            call_shim(&mut host, &mut memory, "AIL_allocate_sample_handle", &[driver]).unwrap(),
        );
        call_shim(&mut host, &mut memory, "AIL_start_sample", &[sample]).unwrap();
        assert!(host.audio.events().is_empty(), "nothing to play, nothing queued");
    }

    #[test]
    fn a_stream_reports_the_length_the_container_gives() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A one-second 8-bit mono 8000 Hz WAV, which is 8000 bytes of data.
        let mut wav = Vec::new();
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(36_u32 + 8_000).to_le_bytes());
        wav.extend_from_slice(b"WAVEfmt ");
        wav.extend_from_slice(&16_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend_from_slice(&1_u16.to_le_bytes());
        wav.extend_from_slice(&8_u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&8_000_u32.to_le_bytes());
        wav.extend(std::iter::repeat_n(0x80u8, 8_000));
        host.files.provide("Audio/track.wav", wav);

        let name = STACK_BASE + 0x9000;
        memory.write_bytes(name, b"Audio\\track.wav\0").unwrap();
        let driver = driver(&mut host, &mut memory);
        let stream = number(
            call_shim(&mut host, &mut memory, "AIL_open_stream", &[driver, name, 0]).unwrap(),
        );
        assert_ne!(stream, 0, "the payload has the file");
        let total = STACK_BASE + 0x9100;
        let current = STACK_BASE + 0x9104;
        call_shim(&mut host, &mut memory, "AIL_stream_ms_position", &[stream, total, current])
            .unwrap();
        assert_eq!(memory.read_u32(total).unwrap(), 1_000);
        assert_eq!(memory.read_u32(current).unwrap(), 0);

        call_shim(&mut host, &mut memory, "AIL_start_stream", &[stream]).unwrap();
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_stream_status", &[stream]).unwrap()),
            SMP_PLAYING
        );
        host.set_clock(500);
        call_shim(&mut host, &mut memory, "AIL_stream_ms_position", &[stream, total, current])
            .unwrap();
        assert_eq!(memory.read_u32(current).unwrap(), 500);
        call_shim(&mut host, &mut memory, "AIL_close_stream", &[stream]).unwrap();
        assert_eq!(host.mss.stream_count(), 0);
    }

    #[test]
    fn a_name_the_payload_does_not_have_is_not_a_stream() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let name = STACK_BASE + 0x9200;
        memory.write_bytes(name, b"no\\such\\track.mp3\0").unwrap();
        let driver = driver(&mut host, &mut memory);
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_open_stream", &[driver, name, 0]).unwrap()),
            0,
            "a missing file is the answer the game checks for"
        );
    }

    #[test]
    fn a_compressed_sample_format_leaves_the_sample_silent() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let driver = driver(&mut host, &mut memory);
        let sample = ready_sample(&mut host, &mut memory, driver, 4_410);
        assert_eq!(
            number(
                call_shim(&mut host, &mut memory, "AIL_set_sample_type", &[sample, 0x50, 0])
                    .unwrap()
            ),
            0,
            "MPEG layer 3 is not a format this host mixes"
        );
        call_shim(&mut host, &mut memory, "AIL_start_sample", &[sample]).unwrap();
        assert!(host.audio.events().is_empty());
    }

    #[test]
    fn a_three_d_sample_plays_and_keeps_its_position() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        // A 3D sample still needs the digital device open, the way the game does it.
        driver(&mut host, &mut memory);
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_open_3D_provider", &[1]).unwrap()),
            M3D_NOERR
        );
        let sample = number(
            call_shim(&mut host, &mut memory, "AIL_allocate_3D_sample_handle", &[1]).unwrap(),
        );
        let data = host.allocate(2_000, 4).unwrap();
        // `AILSOUNDINFO`: format, data, length, rate, bits, channels.
        let info = STACK_BASE + 0x9300;
        for (index, value) in [1, data, 2_000, 22_050, 16, 1].iter().enumerate() {
            memory.write_u32(info + index as u32 * 4, *value).unwrap();
        }
        assert_eq!(
            number(
                call_shim(&mut host, &mut memory, "AIL_set_3D_sample_info", &[sample, info])
                    .unwrap()
            ),
            1
        );
        let (x, y, z) = (1.0_f32, 2.0, 3.0);
        for (index, value) in [x, y, z].iter().enumerate() {
            memory
                .write_u32(info + 0x40 + index as u32 * 4, value.to_bits())
                .unwrap();
        }
        let position = [
            memory.read_u32(info + 0x40).unwrap(),
            memory.read_u32(info + 0x44).unwrap(),
            memory.read_u32(info + 0x48).unwrap(),
        ];
        call_shim(
            &mut host,
            &mut memory,
            "AIL_set_3D_position",
            &[sample, position[0], position[1], position[2]],
        )
        .unwrap();
        call_shim(&mut host, &mut memory, "AIL_start_3D_sample", &[sample]).unwrap();
        assert_eq!(host.audio.events().len(), 1);
        assert_eq!(
            number(call_shim(&mut host, &mut memory, "AIL_3D_sample_status", &[sample]).unwrap()),
            SMP_PLAYING
        );
        let recorded = host.mss.samples.get(&sample).expect("the sample exists");
        assert_eq!(recorded.position, [x, y, z]);
    }

    #[test]
    fn a_second_error_reuses_the_buffer_it_handed_out() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        call_shim(&mut host, &mut memory, "AIL_mem_alloc_lock", &[0]).unwrap();
        let first = number(call_shim(&mut host, &mut memory, "AIL_last_error", &[]).unwrap());
        let text = memory.read_cstr(first).unwrap();
        assert_eq!(text, "no error");
        let name = STACK_BASE + 0x9400;
        memory.write_bytes(name, b"missing.mp3\0").unwrap();
        let driver = driver(&mut host, &mut memory);
        call_shim(&mut host, &mut memory, "AIL_open_stream", &[driver, name, 0]).unwrap();
        let second = number(call_shim(&mut host, &mut memory, "AIL_last_error", &[]).unwrap());
        assert_eq!(second, first, "the same buffer, rewritten");
        assert!(memory.read_cstr(second).unwrap().contains("missing.mp3"));
    }

    #[test]
    fn the_volume_and_pan_scales_land_where_the_mixer_expects() {
        assert_eq!(volume_of(127), 0);
        assert!(volume_of(64) < 0 && volume_of(64) > -10_000);
        assert_eq!(volume_of(0), -10_000);
        assert_eq!(pan_of(64), 0);
        assert_eq!(pan_of(0), -10_000);
        assert_eq!(pan_of(127), 10_000);
    }
}
