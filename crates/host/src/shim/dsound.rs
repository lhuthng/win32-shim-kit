//! `DSOUND.dll`: the audio device the game creates before it touches DirectDraw.
//!
//! The guest uses a small part of DirectSound: one primary buffer, then one
//! secondary buffer per sound. It fills a secondary buffer by locking the whole
//! thing, copying a WAV file's samples into the pointer, unlocking, setting the
//! volume and pan, and calling Play with `DSBPLAY_LOOPING` for music. Nothing
//! streams: the play routine at `0x004892fe` copies the complete file and starts
//! it, so a buffer's samples are final by the time Play is called.
//!
//! That shape makes this shim simple: allocate `dwBufferBytes` of guest memory
//! for each secondary buffer, snapshot those bytes at Play, and publish an event
//! the page hands to its AudioWorklet. The mixer -- resampling, volume, pan and
//! looping -- lives in JavaScript, next to the audio hardware.
//!
//! # Status has to match exactly
//!
//! The guest's cleanup routine at `0x0048989e` compares `GetStatus` against `1`
//! (`DSBSTATUS_PLAYING`) and `4` (`DSBSTATUS_LOOPING`) with equality, not a bit
//! test. Returning `PLAYING | LOOPING` would look like neither and the guest
//! would stop every looping sound on the next frame, so this host returns the
//! single bit the guest expects.

use std::collections::HashMap;

use crate::shim::{Action, Binding, Call, Host};
use crate::thunks::{self, m, Method};
use crate::Result;

use Action::Return;

const DSOUND: &str = "DSOUND.dll";

const DS_OK: u32 = 0;
const DSERR_INVALIDPARAM: u32 = 0x8878_0096;
const E_NOINTERFACE: u32 = 0x8000_4002;

/// `DSBSTATUS_*`: what `GetStatus` reports.
pub const DSBSTATUS_PLAYING: u32 = 0x1;
pub const DSBSTATUS_LOOPING: u32 = 0x4;
/// `DSBPLAY_LOOPING`: the only play flag the guest uses.
const DSBPLAY_LOOPING: u32 = 0x1;
/// `DSBCAPS_PRIMARYBUFFER`.
const DSBCAPS_PRIMARYBUFFER: u32 = 0x1;

/// `DSBUFFERDESC` offsets.
const DESC_SIZE: u32 = 0;
const DESC_FLAGS: u32 = 4;
const DESC_BYTES: u32 = 8;
const DESC_FORMAT: u32 = 16;
/// The descriptor the guest builds is 20 bytes.
const DESC_BYTES_TOTAL: u32 = 20;

/// `WAVEFORMATEX` offsets and size.
const WFX_TAG: u32 = 0;
const WFX_CHANNELS: u32 = 2;
const WFX_RATE: u32 = 4;
const WFX_ALIGN: u32 = 12;
const WFX_BITS: u32 = 14;
const WFX_BYTES: u32 = 18;
const WAVE_FORMAT_PCM: u16 = 1;

/// The largest secondary buffer this host accepts. The files are WAVs, not
/// archives, and a non-WAV descriptor should fail loudly rather than eat the
/// arena.
const MAX_SOUND_BYTES: u32 = 64 << 20;

/// Event kinds the page reads through `wh_audio_events_*`.
pub const EVENT_PLAY: u32 = 0;
pub const EVENT_UPDATE: u32 = 1;
pub const EVENT_STOP: u32 = 2;
pub const EVENT_RELEASE: u32 = 3;
/// The guest wrote new samples into a playing buffer. The page copies them
/// again and replaces the voice's PCM, which is how `BGM.dat` streaming keeps
/// feeding the mixer past the first snapshot.
pub const EVENT_DATA: u32 = 4;
/// Event record: eleven little-endian dwords.
pub const EVENT_WORDS: usize = 11;

/// One record the page reads. `#[repr(C)]` keeps the field order the page's
/// `Uint32Array` view assumes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioEvent {
    pub kind: u32,
    pub voice: u32,
    pub channels: u32,
    pub rate: u32,
    pub frames: u32,
    pub volume: i32,
    pub pan: i32,
    /// Bit 0: looping.
    pub flags: u32,
    /// PCM bytes, so the page can size its copy before asking for the pointer.
    pub bytes: u32,
    /// Playback rate in Hz, which `SetFrequency` can change.
    pub frequency: u32,
    /// Bits per sample: 8 or 16.
    pub bits: u32,
}

#[derive(Clone, Copy, Debug)]
struct Format {
    channels: u16,
    rate: u32,
    align: u16,
    bits: u16,
}

impl Default for Format {
    fn default() -> Self {
        Self {
            channels: 2,
            rate: 22_050,
            align: 4,
            bits: 16,
        }
    }
}

struct SoundBuffer {
    refs: u32,
    primary: bool,
    /// Bytes the guest asked for and the guest address holding them. Zero for
    /// the primary buffer, which has no samples.
    size: u32,
    data: u32,
    format: Format,
    volume: i32,
    pan: i32,
    frequency: u32,
    playing: bool,
    looping: bool,
    /// Host clock reading when Play began, and the byte offset it began at.
    started_ms: u64,
    start_bytes: u32,
    /// Samples copied at Play. The guest may release the buffer while the page
    /// still has the Play event queued, so the event has to own its bytes.
    pcm: Vec<u8>,
}

impl SoundBuffer {
    fn frame_bytes(&self) -> u32 {
        u32::from(self.format.align).max(1)
    }

    fn frames(&self) -> u32 {
        self.size / self.frame_bytes()
    }

    /// How long a non-looping play lasts, in milliseconds.
    fn duration_ms(&self) -> u64 {
        let rate = u64::from(self.format.rate.max(1));
        let frames = u64::from(self.frames());
        frames.saturating_mul(1000) / rate
    }
}

/// Everything `DirectSoundCreate` leaves behind.
pub struct DirectSound {
    device: Option<u32>,
    primary: Option<u32>,
    buffers: HashMap<u32, SoundBuffer>,
    /// Buffers whose Release event has not been read yet. Their PCM has to
    /// outlive the guest object so the page can copy it when it processes the
    /// queued Play.
    retired: HashMap<u32, SoundBuffer>,
    events: Vec<AudioEvent>,
    /// How many sounds the guest started, for the page's status line and tests.
    plays: u64,
    cooperative_level: u32,
}

impl Default for DirectSound {
    fn default() -> Self {
        Self {
            device: None,
            primary: None,
            buffers: HashMap::new(),
            retired: HashMap::new(),
            events: Vec::new(),
            plays: 0,
            cooperative_level: 0,
        }
    }
}

impl DirectSound {
    pub fn is_open(&self) -> bool {
        self.device.is_some()
    }

    pub fn plays(&self) -> u64 {
        self.plays
    }

    pub fn voice_count(&self) -> usize {
        self.buffers.values().filter(|buffer| !buffer.primary).count()
    }

    pub fn events(&self) -> &[AudioEvent] {
        &self.events
    }

    /// The samples for a voice, whether it is still alive or only waiting for
    /// the page to consume its Release event.
    pub fn pcm(&self, voice: u32) -> &[u8] {
        self.buffers
            .get(&voice)
            .or_else(|| self.retired.get(&voice))
            .map_or(&[], |buffer| buffer.pcm.as_slice())
    }

    /// The page has processed every event, so a released voice's samples can go.
    pub fn clear_events(&mut self) {
        self.events.clear();
        self.retired.clear();
    }
}

/// `IDirectSound`, in vtable order.
pub(crate) const SOUND_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("CreateSoundBuffer", 16),
    m("GetCaps", 8),
    m("DuplicateSoundBuffer", 12),
    m("SetCooperativeLevel", 12),
    m("Compact", 4),
    m("GetSpeakerConfig", 8),
    m("SetSpeakerConfig", 8),
    m("Initialize", 12),
];

/// `IDirectSoundBuffer`, in vtable order. The offsets match DirectX 5:
/// `Lock` 0x2C, `Play` 0x30, `SetCurrentPosition` 0x34, `SetFormat` 0x38,
/// `SetVolume` 0x3C, `SetPan` 0x40, `SetFrequency` 0x44, `Stop` 0x48,
/// `Unlock` 0x4C.
pub(crate) const BUFFER_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("GetCaps", 8),
    m("GetCurrentPosition", 12),
    m("GetFormat", 12),
    m("GetVolume", 8),
    m("GetPan", 8),
    m("GetFrequency", 8),
    m("GetStatus", 8),
    m("Initialize", 12),
    m("Lock", 32),
    m("Play", 16),
    m("SetCurrentPosition", 8),
    m("SetFormat", 8),
    m("SetVolume", 8),
    m("SetPan", 8),
    m("SetFrequency", 8),
    m("Stop", 4),
    m("Unlock", 20),
];

pub const SHIMS: &[Binding] = &[
    // Ordinal 1: `DirectSoundCreate(LPGUID, LPDIRECTSOUND *, LPUNKNOWN)`.
    Binding { dll: DSOUND, name: "#1", arg_bytes: 12, shim: direct_sound_create },
];

fn direct_sound_create(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let destination = call.arg(1)?;
    let outer = call.arg(2)?;
    if outer != 0 {
        return Ok(Return(E_NOINTERFACE));
    }
    if destination == 0 {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    let object = thunks::new_object(call.memory, host, thunks::DSOUND)?;
    host.audio.device = Some(object);
    call.write_u32(destination, object)?;
    host.notice(
        "DirectSound: device created (primary buffer, 8 mixing slots, 22050 Hz)".to_string(),
    );
    Ok(Return(DS_OK))
}

pub(crate) fn serve(
    host: &mut Host,
    call: &mut Call<'_>,
    kind: u8,
    name: &'static str,
) -> Result<Action> {
    match (kind, name) {
        (_, "QueryInterface") => Ok(Return(E_NOINTERFACE)),
        (_, "AddRef") => Ok(Return(add_ref(host, call.arg(0)?))),
        (_, "Release") => Ok(Return(release(host, call.arg(0)?))),

        // ---- IDirectSound -------------------------------------------------
        (thunks::DSOUND, "CreateSoundBuffer") => create_sound_buffer(host, call),
        (thunks::DSOUND, "SetCooperativeLevel") => {
            host.audio.cooperative_level = call.arg(2)?;
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND, "GetCaps") => {
            let caps = call.arg(1)?;
            if caps != 0 {
                call.zero(caps, 96)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND, "GetSpeakerConfig") => {
            let config = call.arg(1)?;
            if config != 0 {
                // DSSPEAKER_STEREO | DSSPEAKER_CONFIG_GEOMETRY_STEREO: plain
                // stereo, which is what the mixer emits.
                call.write_u32(config, 2)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND, "Compact") | (thunks::DSOUND, "SetSpeakerConfig") => Ok(Return(DS_OK)),

        // ---- IDirectSoundBuffer -------------------------------------------
        (thunks::DSOUND_BUFFER, "Lock") => lock(host, call),
        (thunks::DSOUND_BUFFER, "Unlock") => unlock(host, call),
        (thunks::DSOUND_BUFFER, "Play") => play(host, call),
        (thunks::DSOUND_BUFFER, "Stop") => {
            let this = call.arg(0)?;
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.playing = false;
                let event = AudioEvent {
                    kind: EVENT_STOP,
                    voice: this,
                    ..AudioEvent::default()
                };
                host.audio.events.push(event);
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "GetStatus") => {
            let this = call.arg(0)?;
            let status = status_of(host, this);
            Ok(Return(status))
        }
        (thunks::DSOUND_BUFFER, "GetCurrentPosition") => {
            let this = call.arg(0)?;
            let play = call.arg(1)?;
            let write = call.arg(2)?;
            let position = position_of(host, this);
            if play != 0 {
                call.write_u32(play, position)?;
            }
            if write != 0 {
                call.write_u32(write, position)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "SetCurrentPosition") => {
            let this = call.arg(0)?;
            let position = call.arg(1)?;
            let now = host.now_ms();
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.start_bytes = position.min(buffer.size);
                buffer.started_ms = now;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "SetVolume") => {
            let this = call.arg(0)?;
            let volume = call.arg(1)? as i32;
            let mut event = None;
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.volume = volume.clamp(-10_000, 0);
                if buffer.playing {
                    event = Some(update_event(buffer, this));
                }
            }
            if let Some(event) = event {
                host.audio.events.push(event);
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "SetPan") => {
            let this = call.arg(0)?;
            let pan = call.arg(1)? as i32;
            let mut event = None;
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.pan = pan.clamp(-10_000, 10_000);
                if buffer.playing {
                    event = Some(update_event(buffer, this));
                }
            }
            if let Some(event) = event {
                host.audio.events.push(event);
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "SetFrequency") => {
            let this = call.arg(0)?;
            let frequency = call.arg(1)?;
            let mut event = None;
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.frequency = frequency;
                if buffer.playing {
                    event = Some(update_event(buffer, this));
                }
            }
            if let Some(event) = event {
                host.audio.events.push(event);
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "GetVolume") => {
            let this = call.arg(0)?;
            let destination = call.arg(1)?;
            if let Some(buffer) = host.audio.buffers.get(&this) {
                if destination != 0 {
                    call.write_u32(destination, buffer.volume as u32)?;
                }
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "GetPan") => {
            let this = call.arg(0)?;
            let destination = call.arg(1)?;
            if let Some(buffer) = host.audio.buffers.get(&this) {
                if destination != 0 {
                    call.write_u32(destination, buffer.pan as u32)?;
                }
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "GetFrequency") => {
            let this = call.arg(0)?;
            let destination = call.arg(1)?;
            if let Some(buffer) = host.audio.buffers.get(&this) {
                if destination != 0 {
                    let frequency = if buffer.frequency == 0 {
                        buffer.format.rate
                    } else {
                        buffer.frequency
                    };
                    call.write_u32(destination, frequency)?;
                }
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "GetCaps") => {
            let this = call.arg(0)?;
            let caps = call.arg(1)?;
            if caps != 0 {
                call.zero(caps, 20)?;
                if let Some(buffer) = host.audio.buffers.get(&this) {
                    call.write_u32(caps, 20)?;
                    call.write_u32(
                        caps + 4,
                        if buffer.primary {
                            DSBCAPS_PRIMARYBUFFER
                        } else {
                            0
                        },
                    )?;
                    call.write_u32(caps + 8, buffer.size)?;
                }
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "GetFormat") => {
            let this = call.arg(0)?;
            let destination = call.arg(1)?;
            let bytes = call.arg(2)?;
            if let Some(buffer) = host.audio.buffers.get(&this) {
                if destination != 0 {
                    let wanted = if bytes == 0 { WFX_BYTES } else { bytes.min(WFX_BYTES) };
                    let mut format = [0_u8; WFX_BYTES as usize];
                    format[WFX_TAG as usize..WFX_TAG as usize + 2]
                        .copy_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
                    format[WFX_CHANNELS as usize..WFX_CHANNELS as usize + 2]
                        .copy_from_slice(&buffer.format.channels.to_le_bytes());
                    format[WFX_RATE as usize..WFX_RATE as usize + 4]
                        .copy_from_slice(&buffer.format.rate.to_le_bytes());
                    format[WFX_ALIGN as usize..WFX_ALIGN as usize + 2]
                        .copy_from_slice(&buffer.format.align.to_le_bytes());
                    format[WFX_BITS as usize..WFX_BITS as usize + 2]
                        .copy_from_slice(&buffer.format.bits.to_le_bytes());
                    call.write(destination, &format[..wanted as usize])?;
                }
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "SetFormat") => {
            let this = call.arg(0)?;
            let format = call.arg(1)?;
            if format != 0 {
                let parsed = read_format(call, format)?;
                if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                    buffer.format = parsed;
                }
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_BUFFER, "Initialize") => Ok(Return(DS_OK)),

        (kind, name) => Err(format!(
            "{}::{name} has no shim: the object exists but this method does",
            thunks::kind_name(kind)
        )),
    }
}

fn add_ref(host: &mut Host, this: u32) -> u32 {
    if let Some(buffer) = host.audio.buffers.get_mut(&this) {
        buffer.refs = buffer.refs.saturating_add(1);
        return buffer.refs;
    }
    if host.audio.device == Some(this) {
        return 1;
    }
    0
}

fn release(host: &mut Host, this: u32) -> u32 {
    if host.audio.device == Some(this) {
        host.audio.device = None;
        return 0;
    }
    let Some(mut buffer) = host.audio.buffers.remove(&this) else {
        return 0;
    };
    buffer.refs = buffer.refs.saturating_sub(1);
    if buffer.refs > 0 {
        let refs = buffer.refs;
        host.audio.buffers.insert(this, buffer);
        return refs;
    }
    if host.audio.primary == Some(this) {
        host.audio.primary = None;
    }
    if !buffer.primary {
        if buffer.data != 0 {
            host.release(buffer.data);
            buffer.data = 0;
        }
        let event = AudioEvent {
            kind: EVENT_RELEASE,
            voice: this,
            ..AudioEvent::default()
        };
        host.audio.events.push(event);
        host.audio.retired.insert(this, buffer);
    }
    0
}

fn read_format(call: &Call<'_>, address: u32) -> Result<Format> {
    let tag = call.memory.read_u16(address + WFX_TAG)?;
    let channels = call.memory.read_u16(address + WFX_CHANNELS)?;
    let rate = call.memory.read_u32(address + WFX_RATE)?;
    let align = call.memory.read_u16(address + WFX_ALIGN)?;
    let bits = call.memory.read_u16(address + WFX_BITS)?;
    if tag != WAVE_FORMAT_PCM {
        return Err(format!(
            "DirectSound: WAV format tag {tag} is not PCM; this host mixes PCM only"
        ));
    }
    if rate == 0 || align == 0 || channels == 0 || channels > 2 {
        return Err(format!(
            "DirectSound: unsupported WAV format ({channels} channels, {rate} Hz, {align}-byte frames)"
        ));
    }
    Ok(Format {
        channels,
        rate,
        align,
        bits,
    })
}

fn create_sound_buffer(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let desc = call.arg(1)?;
    let destination = call.arg(2)?;
    let outer = call.arg(3)?;
    if outer != 0 {
        return Ok(Return(E_NOINTERFACE));
    }
    if host.audio.device != Some(this) {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    if desc == 0 || destination == 0 {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    let size = call.memory.read_u32(desc + DESC_SIZE)?;
    if size < DESC_BYTES_TOTAL {
        return Err(format!(
            "DirectSound: a {size}-byte DSBUFFERDESC is smaller than the 20-byte structure"
        ));
    }
    let flags = call.memory.read_u32(desc + DESC_FLAGS)?;
    let primary = flags & DSBCAPS_PRIMARYBUFFER != 0;
    let bytes = call.memory.read_u32(desc + DESC_BYTES)?;
    let format_address = call.memory.read_u32(desc + DESC_FORMAT)?;

    if !primary {
        if bytes == 0 || bytes > MAX_SOUND_BYTES {
            return Err(format!(
                "DirectSound: a {bytes}-byte sound buffer is outside what this host mixes"
            ));
        }
        if format_address == 0 {
            return Ok(Return(DSERR_INVALIDPARAM));
        }
    }

    let format = if format_address != 0 {
        read_format(call, format_address)?
    } else {
        Format::default()
    };
    let data = if primary { 0 } else { host.allocate(bytes, 4)? };
    let object = thunks::new_object(call.memory, host, thunks::DSOUND_BUFFER)?;
    let buffer = SoundBuffer {
        refs: 1,
        primary,
        size: if primary { 0 } else { bytes },
        data,
        format,
        volume: 0,
        pan: 0,
        frequency: 0,
        playing: false,
        looping: false,
        started_ms: 0,
        start_bytes: 0,
        pcm: Vec::new(),
    };
    host.audio.buffers.insert(object, buffer);
    if primary {
        host.audio.primary = Some(object);
    }
    call.write_u32(destination, object)?;
    Ok(Return(DS_OK))
}

fn lock(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let offset = call.arg(1)?;
    let requested = call.arg(2)?;
    let pointer1 = call.arg(3)?;
    let bytes1 = call.arg(4)?;
    let pointer2 = call.arg(5)?;
    let bytes2 = call.arg(6)?;
    let Some(buffer) = host.audio.buffers.get(&this) else {
        return Ok(Return(DSERR_INVALIDPARAM));
    };
    if buffer.primary {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    if offset > buffer.size {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    let length = if requested == 0 {
        buffer.size - offset
    } else {
        requested
    };
    if length > buffer.size - offset {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    let address = buffer.data.saturating_add(offset);
    if pointer1 != 0 {
        call.write_u32(pointer1, address)?;
    }
    if bytes1 != 0 {
        call.write_u32(bytes1, length)?;
    }
    // The whole buffer fits in one run: DirectSound may split a looping lock at
    // the end of the buffer, but this host hands out the contiguous range.
    if pointer2 != 0 {
        call.write_u32(pointer2, 0)?;
    }
    if bytes2 != 0 {
        call.write_u32(bytes2, 0)?;
    }
    Ok(Return(DS_OK))
}

/// Publishes the samples the guest wrote while the buffer was locked.
///
/// A streaming voice (`BGM.dat`) locks one half of a looping buffer, decodes
/// into it, and unlocks; without this event the mixer would loop the snapshot
/// taken at Play forever. The whole buffer is copied, not the unlocked range:
/// the page replaces the voice's sample array in place, so its play position
/// stays where the mixer left it.
fn unlock(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let Some(buffer) = host.audio.buffers.get(&this) else {
        return Ok(Return(DS_OK));
    };
    if buffer.primary || !buffer.playing || buffer.size == 0 || buffer.data == 0 {
        return Ok(Return(DS_OK));
    }
    let pcm = call
        .memory
        .read_bytes(buffer.data, buffer.size as usize)?
        .to_vec();
    let event = {
        let buffer = host
            .audio
            .buffers
            .get_mut(&this)
            .expect("the buffer was just read");
        buffer.pcm = pcm;
        data_event(buffer, this)
    };
    host.audio.events.push(event);
    Ok(Return(DS_OK))
}

fn play(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {    let this = call.arg(0)?;
    let flags = call.arg(3)?;
    let Some(buffer) = host.audio.buffers.get(&this) else {
        return Ok(Return(DSERR_INVALIDPARAM));
    };
    if buffer.primary || buffer.data == 0 || buffer.size == 0 {
        return Ok(Return(DS_OK));
    }
    let pcm = call.memory.read_bytes(buffer.data, buffer.size as usize)?.to_vec();
    let now = host.now_ms();
    host.audio.plays += 1;
    let event = {
        let buffer = host.audio.buffers.get_mut(&this).expect("the buffer was just read");
        buffer.playing = true;
        buffer.looping = flags & DSBPLAY_LOOPING != 0;
        buffer.started_ms = now;
        buffer.start_bytes = 0;
        buffer.pcm = pcm;
        play_event(buffer, this)
    };
    host.audio.events.push(event);
    Ok(Return(DS_OK))
}

fn play_event(buffer: &SoundBuffer, voice: u32) -> AudioEvent {
    AudioEvent {
        kind: EVENT_PLAY,
        voice,
        channels: u32::from(buffer.format.channels),
        rate: buffer.format.rate,
        frames: buffer.frames(),
        volume: buffer.volume,
        pan: buffer.pan,
        flags: u32::from(buffer.looping),
        bytes: buffer.size,
        frequency: if buffer.frequency == 0 { buffer.format.rate } else { buffer.frequency },
        bits: u32::from(buffer.format.bits),
    }
}

fn data_event(buffer: &SoundBuffer, voice: u32) -> AudioEvent {
    AudioEvent {
        kind: EVENT_DATA,
        voice,
        channels: u32::from(buffer.format.channels),
        rate: buffer.format.rate,
        frames: buffer.frames(),
        flags: u32::from(buffer.looping),
        bytes: buffer.size,
        bits: u32::from(buffer.format.bits),
        ..AudioEvent::default()
    }
}

fn update_event(buffer: &SoundBuffer, voice: u32) -> AudioEvent {
    AudioEvent {
        kind: EVENT_UPDATE,
        voice,
        channels: u32::from(buffer.format.channels),
        volume: buffer.volume,
        pan: buffer.pan,
        frequency: if buffer.frequency == 0 { buffer.format.rate } else { buffer.frequency },
        bits: u32::from(buffer.format.bits),
        ..AudioEvent::default()
    }
}

fn status_of(host: &mut Host, this: u32) -> u32 {
    let now = host.now_ms();
    let Some(buffer) = host.audio.buffers.get_mut(&this) else {
        return 0;
    };
    if !buffer.playing {
        return 0;
    }
    if buffer.looping {
        return DSBSTATUS_LOOPING;
    }
    let elapsed = now.saturating_sub(buffer.started_ms);
    if elapsed < buffer.duration_ms() {
        DSBSTATUS_PLAYING
    } else {
        buffer.playing = false;
        0
    }
}

fn position_of(host: &Host, this: u32) -> u32 {
    let Some(buffer) = host.audio.buffers.get(&this) else {
        return 0;
    };
    if !buffer.playing || buffer.format.rate == 0 {
        return buffer.start_bytes;
    }
    let elapsed = host.now_ms().saturating_sub(buffer.started_ms);
    let bytes = (elapsed as u128)
        .saturating_mul(buffer.format.rate as u128)
        .saturating_mul(buffer.frame_bytes() as u128)
        / 1000;
    let mut position = (buffer.start_bytes as u128).saturating_add(bytes);
    if buffer.size != 0 {
        position %= buffer.size as u128;
    }
    position.min(u128::from(u32::MAX)) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    fn invoke<'a>(memory: &'a mut GuestMemory, args: &[u32]) -> Call<'a> {
        let address = STACK_BASE + 0x6000;
        let mut bytes = Vec::new();
        for argument in args {
            bytes.extend_from_slice(&argument.to_le_bytes());
        }
        memory.write_bytes(address, &bytes).unwrap();
        Call {
            memory,
            args: address,
            arg_bytes: args.len() as u32 * 4,
        }
    }

    fn open(host: &mut Host, memory: &mut GuestMemory) -> u32 {
        let destination = STACK_BASE + 0x7000;
        let mut call = invoke(memory, &[0, destination, 0]);
        assert_eq!(direct_sound_create(host, &mut call).unwrap(), Return(DS_OK));
        memory.read_u32(destination).unwrap()
    }

    fn create_secondary(
        host: &mut Host,
        memory: &mut GuestMemory,
        device: u32,
        bytes: u32,
        channels: u16,
        rate: u32,
    ) -> u32 {
        let desc = STACK_BASE + 0x7100;
        let format = STACK_BASE + 0x7200;
        let out = STACK_BASE + 0x7300;
        memory.write_u32(desc, DESC_BYTES_TOTAL).unwrap();
        memory.write_u32(desc + DESC_FLAGS, 0xe0).unwrap();
        memory.write_u32(desc + DESC_BYTES, bytes).unwrap();
        memory.write_u32(desc + DESC_FORMAT, format).unwrap();
        memory.write_u16(format + WFX_TAG, WAVE_FORMAT_PCM).unwrap();
        memory.write_u16(format + WFX_CHANNELS, channels).unwrap();
        memory.write_u32(format + WFX_RATE, rate).unwrap();
        memory
            .write_u16(format + WFX_ALIGN, channels * 2)
            .unwrap();
        memory.write_u16(format + WFX_BITS, 16).unwrap();
        let mut call = invoke(memory, &[device, desc, out, 0]);
        assert_eq!(
            serve(host, &mut call, thunks::DSOUND, "CreateSoundBuffer").unwrap(),
            Return(DS_OK)
        );
        memory.read_u32(out).unwrap()
    }

    #[test]
    fn the_event_record_is_the_ten_words_the_page_reads() {
        // Every field is a dword, so the page's Uint32Array view has no padding
        // to skip.
        assert_eq!(std::mem::size_of::<AudioEvent>(), EVENT_WORDS * 4);
    }

    #[test]
    fn creating_a_device_hands_back_an_object() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        assert_ne!(device, 0);
        assert!(host.audio.is_open());
        assert_eq!(host.audio.device, Some(device));
    }

    #[test]
    fn a_secondary_buffer_locks_its_whole_range_and_plays_it() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.set_clock(1_000);
        let device = open(&mut host, &mut memory);
        let buffer = create_secondary(&mut host, &mut memory, device, 8, 1, 22_050);
        let pointer = STACK_BASE + 0x7400;
        let length = STACK_BASE + 0x7404;
        let mut call = invoke(
            &mut memory,
            &[buffer, 0, 8, pointer, length, 0, 0, 0],
        );
        assert_eq!(
            serve(&mut host, &mut call, thunks::DSOUND_BUFFER, "Lock").unwrap(),
            Return(DS_OK)
        );
        let address = call.memory.read_u32(pointer).unwrap();
        assert_eq!(call.memory.read_u32(length).unwrap(), 8);
        call.memory
            .write_bytes(address, &[1, 2, 3, 4, 5, 6, 7, 8])
            .unwrap();
        drop(call);
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 1]);
        assert_eq!(
            serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap(),
            Return(DS_OK)
        );
        let event = host.audio.events().last().copied().unwrap();
        assert_eq!(event.kind, EVENT_PLAY);
        assert_eq!(event.channels, 1);
        assert_eq!(event.rate, 22_050);
        assert_eq!(event.flags, 1);
        assert_eq!(host.audio.pcm(buffer), &[1, 2, 3, 4, 5, 6, 7, 8]);
        // Looping reports exactly 4, which is what the guest compares against.
        let mut status = invoke(&mut memory, &[buffer, 0]);
        assert_eq!(
            serve(
                &mut host,
                &mut status,
                thunks::DSOUND_BUFFER,
                "GetStatus"
            )
            .unwrap(),
            Return(DSBSTATUS_LOOPING)
        );
    }

    #[test]
    fn a_one_shot_reports_playing_and_then_finishes() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.set_clock(1_000);
        let device = open(&mut host, &mut memory);
        // 22050 frames at 22050 Hz is one second.
        let buffer = create_secondary(&mut host, &mut memory, device, 44_100, 1, 22_050);
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 0]);
        assert_eq!(
            serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap(),
            Return(DS_OK)
        );
        let mut status = invoke(&mut memory, &[buffer, 0]);
        assert_eq!(
            serve(&mut host, &mut status, thunks::DSOUND_BUFFER, "GetStatus").unwrap(),
            Return(DSBSTATUS_PLAYING)
        );
        host.set_clock(2_500);
        let mut status = invoke(&mut memory, &[buffer, 0]);
        assert_eq!(
            serve(&mut host, &mut status, thunks::DSOUND_BUFFER, "GetStatus").unwrap(),
            Return(0)
        );
    }

    #[test]
    fn volume_and_pan_reach_the_mixer_as_updates() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        let buffer = create_secondary(&mut host, &mut memory, device, 4, 1, 22_050);
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 0]);
        serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap();
        let mut volume = invoke(&mut memory, &[buffer, (-1_500_i32) as u32]);
        serve(&mut host, &mut volume, thunks::DSOUND_BUFFER, "SetVolume").unwrap();
        let mut pan = invoke(&mut memory, &[buffer, (-4_000_i32) as u32]);
        serve(&mut host, &mut pan, thunks::DSOUND_BUFFER, "SetPan").unwrap();
        let updates: Vec<_> = host
            .audio
            .events()
            .iter()
            .filter(|event| event.kind == EVENT_UPDATE)
            .collect();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[1].volume, -1_500);
        assert_eq!(updates[1].pan, -4_000);
    }

    #[test]
    fn releasing_a_played_voice_keeps_its_samples_until_the_page_reads_them() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        let buffer = create_secondary(&mut host, &mut memory, device, 4, 1, 22_050);
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 0]);
        serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap();
        let mut release = invoke(&mut memory, &[buffer]);
        assert_eq!(
            serve(&mut host, &mut release, thunks::DSOUND_BUFFER, "Release").unwrap(),
            Return(0)
        );
        assert_eq!(host.audio.pcm(buffer).len(), 4);
        assert_eq!(host.audio.events().last().unwrap().kind, EVENT_RELEASE);
        host.audio.clear_events();
        assert!(host.audio.pcm(buffer).is_empty());
    }

    #[test]
    fn unlocking_a_playing_buffer_publishes_the_new_samples() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        let buffer = create_secondary(&mut host, &mut memory, device, 8, 1, 22_050);
        let pointer = STACK_BASE + 0x7400;
        let length = STACK_BASE + 0x7404;
        let mut first = invoke(&mut memory, &[buffer, 0, 8, pointer, length, 0, 0, 0]);
        assert_eq!(
            serve(&mut host, &mut first, thunks::DSOUND_BUFFER, "Lock").unwrap(),
            Return(DS_OK)
        );
        let address = first.memory.read_u32(pointer).unwrap();
        first
            .memory
            .write_bytes(address, &[1, 2, 3, 4, 5, 6, 7, 8])
            .unwrap();
        drop(first);
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 1]);
        assert_eq!(
            serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap(),
            Return(DS_OK)
        );
        let mut second = invoke(&mut memory, &[buffer, 0, 8, pointer, length, 0, 0, 0]);
        assert_eq!(
            serve(&mut host, &mut second, thunks::DSOUND_BUFFER, "Lock").unwrap(),
            Return(DS_OK)
        );
        let address = second.memory.read_u32(pointer).unwrap();
        second
            .memory
            .write_bytes(address, &[9, 10, 11, 12, 13, 14, 15, 16])
            .unwrap();
        drop(second);
        let mut unlock = invoke(&mut memory, &[buffer, 0, 0, 0, 0]);
        assert_eq!(
            serve(&mut host, &mut unlock, thunks::DSOUND_BUFFER, "Unlock").unwrap(),
            Return(DS_OK)
        );
        assert_eq!(host.audio.pcm(buffer), &[9, 10, 11, 12, 13, 14, 15, 16]);
        let event = host.audio.events().last().copied().unwrap();
        assert_eq!(event.kind, EVENT_DATA);
        assert_eq!(event.voice, buffer);
        assert_eq!(event.flags, 1);
        assert_eq!(event.bytes, 8);
        assert_eq!(event.bits, 16);
    }

    #[test]
    fn the_primary_buffer_has_no_samples_and_setting_its_format_succeeds() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        let desc = STACK_BASE + 0x7500;
        let format = STACK_BASE + 0x7600;
        let out = STACK_BASE + 0x7700;
        memory.write_u32(desc, DESC_BYTES_TOTAL).unwrap();
        memory.write_u32(desc + DESC_FLAGS, DSBCAPS_PRIMARYBUFFER).unwrap();
        memory.write_u32(desc + DESC_FORMAT, format).unwrap();
        memory.write_u16(format + WFX_TAG, WAVE_FORMAT_PCM).unwrap();
        memory.write_u16(format + WFX_CHANNELS, 2).unwrap();
        memory.write_u32(format + WFX_RATE, 22_050).unwrap();
        memory.write_u16(format + WFX_ALIGN, 4).unwrap();
        memory.write_u16(format + WFX_BITS, 16).unwrap();
        let mut create = invoke(&mut memory, &[device, desc, out, 0]);
        assert_eq!(
            serve(&mut host, &mut create, thunks::DSOUND, "CreateSoundBuffer").unwrap(),
            Return(DS_OK)
        );
        let primary = create.memory.read_u32(out).unwrap();
        assert_eq!(host.audio.primary, Some(primary));
        let mut set = invoke(&mut memory, &[primary, format]);
        assert_eq!(
            serve(&mut host, &mut set, thunks::DSOUND_BUFFER, "SetFormat").unwrap(),
            Return(DS_OK)
        );
    }

    #[test]
    fn a_non_pcm_format_is_refused_with_a_reason() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let _device = open(&mut host, &mut memory);
        let format = STACK_BASE + 0x7800;
        memory.write_u16(format + WFX_TAG, 2).unwrap();
        let call = invoke(&mut memory, &[format]);
        let error = read_format(&call, call.arg(0).unwrap()).unwrap_err();
        assert!(error.contains("not PCM"), "{error}");
    }
}
