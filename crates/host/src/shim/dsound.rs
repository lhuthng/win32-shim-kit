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
//! # Status is a bit set, and guests test the bits
//!
//! `GetStatus` answers `DSBSTATUS_PLAYING | DSBSTATUS_LOOPING` for a buffer that
//! is playing and looping, which is what a real device answers: a looping
//! buffer is still a playing one.
//!
//! This host used to answer `DSBSTATUS_LOOPING` alone, on the theory that a
//! guest comparing with equality would be confused by a value it had not been
//! told about. Hercules is the counter-example. Its per-frame sound update at
//! `0x0040f820` reads the status and then tests one bit of it:
//!
//! ```text
//! 40f877: call [ecx + 0x24]           ; GetStatus(&status)
//! 40f87a: test byte [esp + 0x10], 1   ; DSBSTATUS_PLAYING
//! 40f87e: jne  0x40f888               ; playing: leave the voice alone
//! 40f880: mov  [esi + 0xb99848], -1   ; finished: forget the voice
//! ```
//!
//! A looping buffer reported as `4` therefore reads as *not playing*. The guest
//! forgets the voice and never calls `Stop` on it again, and because nothing but
//! a stop can end a looping buffer, the sound it started plays for the rest of
//! the session. Two of them were audible in the menu: 17,318-byte 11,025 Hz
//! loops, played with `DSBPLAY_LOOPING`, never stopped and never released.
//!
//! Nothing here needs the equality reading. Cave Story never asks for a status
//! at all -- `GetStatus` is not called anywhere in its sound code -- and the
//! only other status test in Hercules is against `DSBSTATUS_BUFFERLOST`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

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
/// `DS_CERTIFIED`: what `VerifyCertification` reports. The alternative,
/// `DS_UNCERTIFIED`, is what a driver without hardware mixing says.
const DS_CERTIFIED: u32 = 0;
/// `KSPROPERTY_SUPPORT_*`: what `QuerySupport` reports about a property set.
const KSPROPERTY_SUPPORT_GET: u32 = 1;
const KSPROPERTY_SUPPORT_SET: u32 = 2;

/// `IID_IUnknown`, the one interface every object offers.
const IID_IUNKNOWN: [u8; 16] = [
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xc0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46,
];
/// `IID_IDirectSoundNotify`
/// `B0210783-89CD-11D0-AF08-00A0C925CD16`, little-endian fields first.
const IID_NOTIFY: [u8; 16] = [
    0x83, 0x07, 0x21, 0xb0, 0xcd, 0x89, 0xd0, 0x11, 0xaf, 0x08, 0x00, 0xa0, 0xc9, 0x25, 0xcd, 0x16,
];
/// `IID_IKsPropertySet`
/// `31EFAC30-515C-11D0-A9AA-00AA0061BE93`, little-endian fields first. This is
/// the interface a guest probes for effect support before it uses it, which is
/// why the QI above hands it a real object.
const IID_IKS_PROPERTYSET: [u8; 16] = [
    0x30, 0xac, 0xef, 0x31, 0x5c, 0x51, 0xd0, 0x11, 0xa9, 0xaa, 0x00, 0xaa, 0x00, 0x61, 0xbe, 0x93,
];
/// `IID_IDirectSound3DBuffer`
/// `279AFA86-4981-11CE-A521-0020AF0BE560`, little-endian fields first.
const IID_3D_BUFFER: [u8; 16] = [
    0x86, 0xfa, 0x9a, 0x27, 0x81, 0x49, 0xce, 0x11, 0xa5, 0x21, 0x00, 0x20, 0xaf, 0x0b, 0xe5, 0x60,
];
/// `IID_IDirectSound3DListener`
/// `279AFA84-4981-11CE-A521-0020AF0BE560`, little-endian fields first.
const IID_3D_LISTENER: [u8; 16] = [
    0x84, 0xfa, 0x9a, 0x27, 0x81, 0x49, 0xce, 0x11, 0xa5, 0x21, 0x00, 0x20, 0xaf, 0x0b, 0xe5, 0x60,
];
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

/// `DSCAPS_*` bits, which say what a device can mix.
const DSCAPS_PRIMARYMONO: u32 = 0x0000_0001;
const DSCAPS_PRIMARYSTEREO: u32 = 0x0000_0002;
const DSCAPS_PRIMARY8BIT: u32 = 0x0000_0004;
const DSCAPS_PRIMARY16BIT: u32 = 0x0000_0008;
const DSCAPS_SECONDARYMONO: u32 = 0x0000_0100;
const DSCAPS_SECONDARYSTEREO: u32 = 0x0000_0200;
const DSCAPS_SECONDARY8BIT: u32 = 0x0000_0400;
const DSCAPS_SECONDARY16BIT: u32 = 0x0000_0800;

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
/// The guest moved a voice's playhead with `SetCurrentPosition`. The byte offset
/// is the event's `bytes`; a mixer keeps its own playhead, so this is the only
/// way it learns that a buffer was rewound (`Stop`, seek to zero, `Play`, which
/// is how Cave Story starts a drum).
pub const EVENT_SEEK: u32 = 5;
/// Event record: eleven little-endian dwords.
pub const EVENT_WORDS: usize = 11;

/// Whether the playback lifecycle is written to the notice trail.
///
/// The trail answers the one question the event stream cannot: *which object a
/// guest aimed a call at*. `Stop` and `Release` are keyed on `this`, and a guest
/// that stops the buffer it did not play looks exactly like a guest that never
/// stopped anything, from the events alone. `SHIMKIT_LOG_AUDIO` cannot be the
/// switch in a browser -- `std::env` does not exist in the wasm module -- so a
/// page turns this on through `wh_audio_trace`.
pub static LOG_AUDIO: AtomicBool = AtomicBool::new(false);

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
    /// The `DSBCAPS_*` flags the guest asked for at creation, which `GetCaps`
    /// reports back: a guest that created a buffer with control flags checks
    /// for them again before it uses the controls.
    caps: u32,
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

    /// The rate the play cursor moves at.
    ///
    /// `SetFrequency` does not only change what is heard: it plays the buffer
    /// itself faster, so its cursor reaches the end sooner and a one-shot stops
    /// sooner. A guest that retunes every note -- which is how Cave Story's
    /// instruments are pitched -- reads the cursor back at this rate.
    fn play_rate(&self) -> u32 {
        if self.frequency == 0 {
            self.format.rate
        } else {
            self.frequency
        }
    }

    /// How long a non-looping play lasts, in milliseconds.
    fn duration_ms(&self) -> u64 {
        let rate = u64::from(self.play_rate().max(1));
        let frames = u64::from(self.frames());
        frames.saturating_mul(1000) / rate
    }
}

/// A voice some other shim mixes through this device.
///
/// `MSS32.dll` (Miles) has its own handle space and its own idea of a sample,
/// but the page has one mixer: the event queue and the two lookups below are
/// the whole contract, so a Miles sample is one more voice in it. `pcm` is a
/// copy this host made; `live` is where the guest's own bytes are, which is what
/// a sample address is -- the guest may rewrite it between frames.
#[derive(Default)]
struct Foreign {
    pcm: Vec<u8>,
    live: Option<(u32, u32)>,
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
    /// Voices the Miles shim started, keyed by the voice number it used.
    foreign: HashMap<u32, Foreign>,
    events: Vec<AudioEvent>,
    /// How many sounds the guest started, for the page's status line and tests.
    plays: u64,
    cooperative_level: u32,
    /// The 3D parameters of every `IDirectSound3DBuffer` handed out, by object.
    /// The page's mixer does not place voices yet, but a guest that sets and
    /// reads back its positions has to see its own numbers.
    spatial: HashMap<u32, Spatial>,
    /// The same for `IDirectSound3DListener`, of which a session has one.
    listeners: HashMap<u32, Listener>,
}

impl Default for DirectSound {
    fn default() -> Self {
        Self {
            device: None,
            primary: None,
            buffers: HashMap::new(),
            retired: HashMap::new(),
            foreign: HashMap::new(),
            events: Vec::new(),
            plays: 0,
            cooperative_level: 0,
            spatial: HashMap::new(),
            listeners: HashMap::new(),
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

    /// Every secondary buffer the guest still holds: its object, whether it is
    /// playing, whether it is looping, and how many bytes it holds.
    ///
    /// The page keeps its own table built from the event stream, so this is the
    /// *guest's* side of the same thing. The two disagreeing is the whole
    /// question when a sound will not stop: a buffer the guest never told to
    /// stop is a stuck voice no matter what the page does with it.
    pub fn voices(&self) -> Vec<(u32, bool, bool, u32)> {
        let mut out: Vec<(u32, bool, bool, u32)> = self
            .buffers
            .iter()
            .filter(|(_, buffer)| !buffer.primary)
            .map(|(voice, buffer)| (*voice, buffer.playing, buffer.looping, buffer.size))
            .collect();
        out.sort_unstable_by_key(|entry| entry.0);
        out
    }

    pub fn events(&self) -> &[AudioEvent] {
        &self.events
    }

    /// The samples for a voice, whether it is still alive or only waiting for
    /// the page to consume its Release event.
    pub fn pcm(&self, voice: u32) -> &[u8] {
        if let Some(buffer) = self.buffers.get(&voice).or_else(|| self.retired.get(&voice)) {
            return buffer.pcm.as_slice();
        }
        // A Miles voice this host copied for the page; a Miles *sample* has no
        // copy at all, because the mixer reads the guest's own bytes.
        match self.foreign.get(&voice) {
            Some(entry) => entry.pcm.as_slice(),
            None => &[],
        }
    }

    /// Where a voice's samples live, in guest memory, and how many bytes the
    /// buffer holds. A guest that keeps the pointer it locked writes into the
    /// buffer while it plays, so a mixer reads here rather than trusting the
    /// copy taken at Play.
    pub fn samples(&self, voice: u32) -> Option<(u32, u32)> {
        if let Some((address, length)) = self.foreign.get(&voice).and_then(|entry| entry.live) {
            return Some((address, length));
        }
        let buffer = self
            .buffers
            .get(&voice)
            .or_else(|| self.retired.get(&voice))?;
        if buffer.primary || buffer.data == 0 {
            return None;
        }
        Some((buffer.data, buffer.size))
    }

    /// Queues a voice whose samples stay in guest memory.
    ///
    /// This is what a Miles sample is: `AIL_set_sample_address` points at bytes
    /// the guest owns and may rewrite, so the mixer has to read there rather
    /// than from a copy taken at `AIL_start_sample`.
    pub fn play_live(&mut self, event: AudioEvent, address: u32, length: u32) {
        self.foreign.entry(event.voice).or_default().live = Some((address, length));
        self.events.push(event);
    }

    /// Queues a voice whose samples this host copied for the page.
    pub fn play_copy(&mut self, event: AudioEvent, pcm: Vec<u8>) {
        self.foreign.entry(event.voice).or_default().pcm = pcm;
        self.events.push(event);
    }

    /// Queues an event that only changes a voice already on the mixer.
    pub fn notify(&mut self, event: AudioEvent) {
        self.events.push(event);
    }

    /// Takes a voice off the mixer and forgets its samples.
    pub fn silence(&mut self, voice: u32) {
        self.foreign.remove(&voice);
    }

    /// The page has processed every event, so a released voice's samples can go.
    pub fn clear_events(&mut self) {
        self.events.clear();
        self.retired.clear();
        // A Miles sample's bytes belong to the guest, so those addresses stay:
        // the voice is still playing after the page has read the event. A copy
        // has been consumed by the time the queue is cleared.
        for entry in self.foreign.values_mut() {
            entry.pcm = Vec::new();
        }
        self.foreign.retain(|_, entry| entry.live.is_some());
    }
}

/// `IDirectSound`, in vtable order, plus the one method `IDirectSound8` adds:
/// a guest that created its device through `DirectSoundCreate8` talks to an
/// `IDirectSound8`, whose first eleven slots are these and whose twelfth asks
/// whether the driver is certified.
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
    m("Initialize", 8),
    m("VerifyCertification", 8),
];

/// `IDirectSoundBuffer`, in vtable order. The offsets match DirectX 5:
/// `Lock` 0x2C, `Play` 0x30, `SetCurrentPosition` 0x34, `SetFormat` 0x38,
/// `SetVolume` 0x3C, `SetPan` 0x40, `SetFrequency` 0x44, `Stop` 0x48,
/// `Unlock` 0x4C, and `Restore` follows them at 0x50.
pub(crate) const BUFFER_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("GetCaps", 8),
    m("GetCurrentPosition", 12),
    m("GetFormat", 16),
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
    m("Restore", 4),
];

/// `IDirectSoundNotify`, in vtable order: the interface a guest pins play
/// events to.
pub(crate) const NOTIFY_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("SetNotificationPositions", 12),
];

/// `IDirectSound3DBuffer`, in vtable order. The getters and setters are paired
/// by name, and every setter ends with an apply flag this host answers
/// immediately, so the deferred mode is answered the same way.
pub(crate) const THREE_D_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("GetAllParameters", 8),
    m("GetConeAngles", 12),
    m("GetConeOrientation", 8),
    m("GetConeOutsideVolume", 8),
    m("GetMaxDistance", 8),
    m("GetMinDistance", 8),
    m("GetMode", 8),
    m("GetPosition", 8),
    m("GetVelocity", 8),
    m("SetAllParameters", 12),
    m("SetConeAngles", 16),
    m("SetConeOrientation", 20),
    m("SetConeOutsideVolume", 12),
    m("SetMaxDistance", 12),
    m("SetMinDistance", 12),
    m("SetMode", 12),
    m("SetPosition", 20),
    m("SetVelocity", 20),
];

/// `IKsPropertySet`, in vtable order: the interface a guest asks what effect
/// property sets the driver supports.
pub(crate) const PROPERTY_SET_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("Get", 32),
    m("Set", 28),
    m("QuerySupport", 16),
];

/// `IDirectSound3DListener`, in vtable order: the primary buffer's 3D twin.
pub(crate) const LISTENER_METHODS: &[Option<Method>] = &[
    m("QueryInterface", 12),
    m("AddRef", 4),
    m("Release", 4),
    m("GetAllParameters", 8),
    m("GetDistanceFactor", 8),
    m("GetDopplerFactor", 8),
    m("GetOrientation", 12),
    m("GetPosition", 8),
    m("GetRolloffFactor", 8),
    m("GetVelocity", 8),
    m("SetAllParameters", 12),
    m("SetDistanceFactor", 12),
    m("SetDopplerFactor", 12),
    m("SetOrientation", 32),
    m("SetPosition", 20),
    m("SetRolloffFactor", 12),
    m("SetVelocity", 20),
    m("CommitDeferredSettings", 4),
];

/// The 3D parameters one `IDirectSound3DListener` carries, stored as raw bits
/// like the buffer's are. The defaults are DirectSound's: no distance scaling,
/// no Doppler, ears at the origin facing forward.
#[derive(Clone, Copy)]
pub struct Listener {
    pub position: [u32; 3],
    pub velocity: [u32; 3],
    pub orientation_front: [u32; 3],
    pub orientation_top: [u32; 3],
    pub distance_factor: u32,
    pub rolloff_factor: u32,
    pub doppler_factor: u32,
}

impl Default for Listener {
    fn default() -> Self {
        Self {
            position: [0; 3],
            velocity: [0; 3],
            // Facing down -z with up along +y, the DirectSound default.
            orientation_front: [0, 0, 0xbf80_0000_u32], // -1.0f
            orientation_top: [0, 0x3f80_0000_u32, 0],   // 1.0f
            distance_factor: 1.0_f32.to_bits(),
            rolloff_factor: 1.0_f32.to_bits(),
            doppler_factor: 1.0_f32.to_bits(),
        }
    }
}

/// The 3D parameters one `IDirectSound3DBuffer` carries, stored as the raw bits
/// a setter took so a getter hands them back unchanged. The defaults are the
/// ones DirectSound documents: centred, facing forward, one metre near, and
/// hearing everything.
#[derive(Clone, Copy)]
pub struct Spatial {
    pub position: [u32; 3],
    pub velocity: [u32; 3],
    pub cone_orientation: [u32; 3],
    pub cone_inside: u32,
    pub cone_outside: u32,
    pub cone_outside_volume: u32,
    pub min_distance: u32,
    pub max_distance: u32,
    pub mode: u32,
}

impl Default for Spatial {
    fn default() -> Self {
        Self {
            position: [0; 3],
            velocity: [0; 3],
            cone_orientation: [0; 3],
            cone_inside: 360,
            cone_outside: 360,
            cone_outside_volume: 0,
            min_distance: 1.0_f32.to_bits(),
            max_distance: 1.0e9_f32.to_bits(),
            mode: 0, // DS3DMODE_NORMAL
        }
    }
}

pub const SHIMS: &[Binding] = &[
    // Ordinal 1: `DirectSoundCreate(LPGUID, LPDIRECTSOUND *, LPUNKNOWN)`.
    Binding { dll: DSOUND, name: "#1", arg_bytes: 12, shim: direct_sound_create },
    // Ordinal 11: `DirectSoundCreate8(LPGUID, LPDIRECTSOUND8 *, LPUNKNOWN)`,
    // the DX8 create a guest imports when it wants the eight-and-later
    // interface. Its first eleven slots are `IDirectSound`'s, so the same
    // device answers both.
    Binding { dll: DSOUND, name: "#11", arg_bytes: 12, shim: direct_sound_create },
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

/// `QueryInterface`: `IID_IUnknown` answers the same object one reference
/// heavier, and the two interfaces a guest upgrades a buffer to are answered
/// with real objects a guest then drives. Everything else is refused the way
/// COM refuses it, with `E_NOINTERFACE`: a guest told an interface exists
/// would call into a vtable this host cannot answer.
fn query_interface(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let riid = call.arg(1)?;
    let destination = call.arg(2)?;
    if destination == 0 {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    let mut iid = [0_u8; 16];
    for (index, bytes) in iid.chunks_exact_mut(4).enumerate() {
        bytes.copy_from_slice(&call.memory.read_u32(riid + index as u32 * 4)?.to_le_bytes());
    }
    let object = if iid == IID_IUNKNOWN {
        add_ref(host, this);
        this
    } else if iid == IID_IKS_PROPERTYSET {
        thunks::new_object(call.memory, host, thunks::DSOUND_PROPERTY_SET)?
    } else if iid == IID_NOTIFY {
        thunks::new_object(call.memory, host, thunks::DSOUND_NOTIFY)?
    } else if iid == IID_3D_BUFFER {
        thunks::new_object(call.memory, host, thunks::DSOUND_3D_BUFFER)?
    } else if iid == IID_3D_LISTENER {
        thunks::new_object(call.memory, host, thunks::DSOUND_3D_LISTENER)?
    } else {
        return Ok(Return(E_NOINTERFACE));
    };
    call.write_u32(destination, object)?;
    Ok(Return(DS_OK))
}

/// `sizeof(DS3DBUFFER)` and where its members sit: the size dword, then the
/// position and velocity vectors, the two cone angles, the cone orientation,
/// the cone's outside volume, the two distances, and the mode.
const DS3DBUFFER_BYTES: u32 = 64;
const W3D_POSITION: u32 = 4;
const W3D_VELOCITY: u32 = 16;
const W3D_CONE_INSIDE: u32 = 28;
const W3D_CONE_OUTSIDE: u32 = 32;
const W3D_CONE_ORIENTATION: u32 = 36;
const W3D_CONE_OUTSIDE_VOLUME: u32 = 48;
const W3D_MIN_DISTANCE: u32 = 52;
const W3D_MAX_DISTANCE: u32 = 56;
const W3D_MODE: u32 = 60;

/// The 3D parameters of an object, created from the documented defaults the
/// first time a guest asks about a buffer it has only positioned pieces of.
fn spatial_of(host: &mut Host, object: u32) -> Spatial {
    *host.audio.spatial.entry(object).or_default()
}

/// Reads a `D3DVECTOR` as three raw float bits.
fn read_vector(call: &Call<'_>, at: u32) -> Result<[u32; 3]> {
    Ok([
        call.memory.read_u32(at)?,
        call.memory.read_u32(at + 4)?,
        call.memory.read_u32(at + 8)?,
    ])
}

/// Writes a `D3DVECTOR` as three raw float bits.
fn write_vector(call: &mut Call<'_>, at: u32, vector: [u32; 3]) -> Result<()> {
    for (index, word) in vector.iter().enumerate() {
        call.write_u32(at + index as u32 * 4, *word)?;
    }
    Ok(())
}

/// Reads a whole `DS3DBUFFER`, which is how `SetAllParameters` sets everything
/// at once.
fn read_spatial(call: &Call<'_>, at: u32) -> Result<Spatial> {
    Ok(Spatial {
        position: read_vector(call, at + W3D_POSITION)?,
        velocity: read_vector(call, at + W3D_VELOCITY)?,
        cone_orientation: read_vector(call, at + W3D_CONE_ORIENTATION)?,
        cone_inside: call.memory.read_u32(at + W3D_CONE_INSIDE)?,
        cone_outside: call.memory.read_u32(at + W3D_CONE_OUTSIDE)?,
        cone_outside_volume: call.memory.read_u32(at + W3D_CONE_OUTSIDE_VOLUME)?,
        min_distance: call.memory.read_u32(at + W3D_MIN_DISTANCE)?,
        max_distance: call.memory.read_u32(at + W3D_MAX_DISTANCE)?,
        mode: call.memory.read_u32(at + W3D_MODE)?,
    })
}

/// Writes a whole `DS3DBUFFER`, which is how `GetAllParameters` answers.
fn write_spatial(call: &mut Call<'_>, at: u32, spatial: &Spatial) -> Result<()> {
    call.write_u32(at, DS3DBUFFER_BYTES)?; // dwSize
    write_vector(call, at + W3D_POSITION, spatial.position)?;
    write_vector(call, at + W3D_VELOCITY, spatial.velocity)?;
    call.write_u32(at + W3D_CONE_INSIDE, spatial.cone_inside)?;
    call.write_u32(at + W3D_CONE_OUTSIDE, spatial.cone_outside)?;
    write_vector(call, at + W3D_CONE_ORIENTATION, spatial.cone_orientation)?;
    call.write_u32(at + W3D_CONE_OUTSIDE_VOLUME, spatial.cone_outside_volume)?;
    call.write_u32(at + W3D_MIN_DISTANCE, spatial.min_distance)?;
    call.write_u32(at + W3D_MAX_DISTANCE, spatial.max_distance)?;
    call.write_u32(at + W3D_MODE, spatial.mode)?;
    Ok(())
}

/// `sizeof(DS3DLISTENER)` and where its members sit: the size dword, then the
/// position and velocity, the two orientation vectors, and the three scaling
/// factors.
const DS3DLISTENER_BYTES: u32 = 64;
const L3D_POSITION: u32 = 4;
const L3D_VELOCITY: u32 = 16;
const L3D_ORIENT_FRONT: u32 = 28;
const L3D_ORIENT_TOP: u32 = 40;
const L3D_DISTANCE_FACTOR: u32 = 52;
const L3D_ROLLOFF_FACTOR: u32 = 56;
const L3D_DOPPLER_FACTOR: u32 = 60;

/// The 3D parameters of a listener, created from the documented defaults the
/// first time a guest asks.
fn listener_of(host: &mut Host, object: u32) -> Listener {
    *host.audio.listeners.entry(object).or_default()
}

/// Reads a whole `DS3DLISTENER`, which is how `SetAllParameters` sets it all.
fn read_listener(call: &Call<'_>, at: u32) -> Result<Listener> {
    Ok(Listener {
        position: read_vector(call, at + L3D_POSITION)?,
        velocity: read_vector(call, at + L3D_VELOCITY)?,
        orientation_front: read_vector(call, at + L3D_ORIENT_FRONT)?,
        orientation_top: read_vector(call, at + L3D_ORIENT_TOP)?,
        distance_factor: call.memory.read_u32(at + L3D_DISTANCE_FACTOR)?,
        rolloff_factor: call.memory.read_u32(at + L3D_ROLLOFF_FACTOR)?,
        doppler_factor: call.memory.read_u32(at + L3D_DOPPLER_FACTOR)?,
    })
}

/// Writes a whole `DS3DLISTENER`, which is how `GetAllParameters` answers.
fn write_listener(call: &mut Call<'_>, at: u32, listener: &Listener) -> Result<()> {
    call.write_u32(at, DS3DLISTENER_BYTES)?; // dwSize
    write_vector(call, at + L3D_POSITION, listener.position)?;
    write_vector(call, at + L3D_VELOCITY, listener.velocity)?;
    write_vector(call, at + L3D_ORIENT_FRONT, listener.orientation_front)?;
    write_vector(call, at + L3D_ORIENT_TOP, listener.orientation_top)?;
    call.write_u32(at + L3D_DISTANCE_FACTOR, listener.distance_factor)?;
    call.write_u32(at + L3D_ROLLOFF_FACTOR, listener.rolloff_factor)?;
    call.write_u32(at + L3D_DOPPLER_FACTOR, listener.doppler_factor)?;
    Ok(())
}

pub(crate) fn serve(
    host: &mut Host,
    call: &mut Call<'_>,
    kind: u8,
    name: &'static str,
) -> Result<Action> {
    // `SHIMKIT_LOG_AUDIO` traces every call into the device, which is the only
    // way to see how a guest drives its mixer: a buffer that is played once and
    // never refilled looks the same as one that is never played at all from the
    // outside, and the two have different causes.
    // The rendering pass creates a buffer per track and locks each one in small
    // pieces, which is thousands of calls: only the playback lifecycle is traced.
    // `SHIMKIT_LOG_AUDIO_ALL` traces every method instead, which says where a
    // guest stops talking to its mixer at all -- a device that is never asked to
    // play anything is a different fault from one that plays silence.
    let every = std::env::var("SHIMKIT_LOG_AUDIO_ALL").is_ok();
    if every {
        let caller = call.memory.read_u32(call.args.wrapping_sub(4)).unwrap_or(0);
        // Straight to stdout, uncapped: an init burst fills the notice list, and
        // the calls after it are the ones worth reading.
        println!(
            "audio {name} args={:08x},{:08x},{:08x},{:08x} at {caller:#010x}",
            call.arg(0).unwrap_or(0),
            call.arg(1).unwrap_or(0),
            call.arg(2).unwrap_or(0),
            call.arg(3).unwrap_or(0),
        );
    }
    // The playback lifecycle, traced only on request. These notices were once
    // unconditional so a browser session could name its audio calls without an
    // env var, and every `Lock` and `Unlock` of a streaming buffer then landed
    // in the page's diagnostic pane -- in the same buffer the page reads a stop
    // reason from, so the last audio call was shown in place of the fault.
    // `GetCurrentPosition` is still left out: a guest that polls it every frame
    // would drown the trail.
    //
    // `LOG_AUDIO` is the way in for a page: `std::env` does not exist in the
    // wasm module, so `var` fails there and the one caller that most needs the
    // trail -- a browser session -- could never ask for it. `native_probe` keeps
    // using the variable, which costs nothing when the flag is already set.
    if (LOG_AUDIO.load(Ordering::Relaxed) || std::env::var("SHIMKIT_LOG_AUDIO").is_ok())
        && host.notices.len() < 200_000
        && matches!(
            name,
            "Play"
                | "Stop"
                | "SetFormat"
                | "Lock"
                | "Unlock"
                | "GetStatus"
                | "SetCurrentPosition"
                | "SetFrequency"
                | "SetVolume"
                | "DuplicateSoundBuffer"
                | "Release"
        )
    {
        // The return address on this call's own stack, so a Play names the
        // routine that started it.
        let caller = call
            .memory
            .read_u32(call.args.wrapping_sub(4))
            .unwrap_or(0);
        host.notice(format!(
            "audio {name} this={:#010x} args={:08x},{:08x},{:08x},{:08x} from {caller:#010x}",
            call.arg(0).unwrap_or(0),
            call.arg(1).unwrap_or(0),
            call.arg(2).unwrap_or(0),
            call.arg(3).unwrap_or(0),
            call.arg(4).unwrap_or(0)
        ));
    }
    let outcome = match (kind, name) {
        (_, "QueryInterface") => query_interface(host, call),
        (_, "AddRef") => Ok(Return(add_ref(host, call.arg(0)?))),
        (_, "Release") => Ok(Return(release(host, call.arg(0)?))),

        // ---- IDirectSoundNotify -------------------------------------------
        // A notification the page never fires is a guest waiting on an event,
        // which the trace says; accepting the positions is what keeps the run
        // going so the next call can name itself.
        (thunks::DSOUND_NOTIFY, "SetNotificationPositions") => Ok(Return(DS_OK)),

        // ---- IDirectSound3DListener ---------------------------------------
        // Stored and answered like the buffer's 3D twin: the ears' position and
        // orientation are the guest's numbers until the mixer places voices.
        (thunks::DSOUND_3D_LISTENER, "GetAllParameters") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            if destination != 0 {
                let listener = listener_of(host, object);
                write_listener(call, destination, &listener)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "SetAllParameters") => {
            let object = call.arg(0)?;
            let source = call.arg(1)?;
            if source != 0 {
                let listener = read_listener(call, source)?;
                host.audio.listeners.insert(object, listener);
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "GetDistanceFactor" | "GetDopplerFactor" | "GetRolloffFactor") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let listener = listener_of(host, object);
            let value = match name {
                "GetDistanceFactor" => listener.distance_factor,
                "GetDopplerFactor" => listener.doppler_factor,
                _ => listener.rolloff_factor,
            };
            if destination != 0 {
                call.write_u32(destination, value)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "SetDistanceFactor" | "SetDopplerFactor" | "SetRolloffFactor") => {
            let object = call.arg(0)?;
            let value = call.arg(1)?;
            let listener = listener_of(host, object);
            let updated = match name {
                "SetDistanceFactor" => Listener {
                    distance_factor: value,
                    ..listener
                },
                "SetDopplerFactor" => Listener {
                    doppler_factor: value,
                    ..listener
                },
                _ => Listener {
                    rolloff_factor: value,
                    ..listener
                },
            };
            host.audio.listeners.insert(object, updated);
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "GetOrientation") => {
            let object = call.arg(0)?;
            let front = call.arg(1)?;
            let top = call.arg(2)?;
            let listener = listener_of(host, object);
            if front != 0 {
                write_vector(call, front, listener.orientation_front)?;
            }
            if top != 0 {
                write_vector(call, top, listener.orientation_top)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "SetOrientation") => {
            let object = call.arg(0)?;
            let front = [call.arg(1)?, call.arg(2)?, call.arg(3)?];
            let top = [call.arg(4)?, call.arg(5)?, call.arg(6)?];
            let listener = listener_of(host, object);
            host.audio.listeners.insert(
                object,
                Listener {
                    orientation_front: front,
                    orientation_top: top,
                    ..listener
                },
            );
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "GetPosition" | "GetVelocity") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let listener = listener_of(host, object);
            let vector = if name == "GetPosition" {
                listener.position
            } else {
                listener.velocity
            };
            if destination != 0 {
                write_vector(call, destination, vector)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_LISTENER, "SetPosition" | "SetVelocity") => {
            let object = call.arg(0)?;
            let vector = [call.arg(1)?, call.arg(2)?, call.arg(3)?];
            let listener = listener_of(host, object);
            let updated = if name == "SetPosition" {
                Listener {
                    position: vector,
                    ..listener
                }
            } else {
                Listener {
                    velocity: vector,
                    ..listener
                }
            };
            host.audio.listeners.insert(object, updated);
            Ok(Return(DS_OK))
        }
        // A deferred parameter batch is applied by every setter already, so
        // committing is a formality.
        (thunks::DSOUND_3D_LISTENER, "CommitDeferredSettings") => Ok(Return(DS_OK)),

        // ---- IKsPropertySet -------------------------------------------------
        // The page's mixer applies no effect properties, so `Get` answers the
        // flat state (everything zeroed) and `Set` accepts and forgets, the way
        // the lighting shims accept a game's lights. Refusing them is what sent
        // this guest down its no-EAX path, which reads a channel this host
        // never filled; the property-set dialogue itself has to succeed for the
        // guest to stay on the KsPropertySet road.
        (thunks::DSOUND_PROPERTY_SET, "Set") => Ok(Return(DS_OK)),
        (thunks::DSOUND_PROPERTY_SET, "Get") => {
            let data = call.arg(5)?;
            let length = call.arg(6)?;
            let written = call.arg(7)?;
            if data != 0 && length != 0 {
                // EAX properties are dwords; zeroing them answers a flat reverb.
                call.zero(data, length.min(256))?;
            }
            if written != 0 {
                call.write_u32(written, if length == 0 { 0 } else { length.min(256) })?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_PROPERTY_SET, "QuerySupport") => {
            let support = call.arg(3)?;
            if support != 0 {
                call.write_u32(
                    support,
                    KSPROPERTY_SUPPORT_GET | KSPROPERTY_SUPPORT_SET,
                )?;
            }
            Ok(Return(DS_OK))
        }

        // ---- IDirectSound3DBuffer -----------------------------------------
        // The page mixes with volume and pan, so these parameters are stored
        // and answered rather than applied: a guest that sets a position and
        // reads it back sees its own numbers, which is what keeps its book-
        // keeping running until the mixer can place voices.
        (thunks::DSOUND_3D_BUFFER, "GetAllParameters") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            if destination != 0 {
                let spatial = spatial_of(host, object);
                write_spatial(call, destination, &spatial)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetAllParameters") => {
            let object = call.arg(0)?;
            let source = call.arg(1)?;
            if source != 0 {
                let spatial = read_spatial(call, source)?;
                host.audio.spatial.insert(object, spatial);
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "GetConeAngles") => {
            let object = call.arg(0)?;
            let inside = call.arg(1)?;
            let outside = call.arg(2)?;
            let spatial = spatial_of(host, object);
            if inside != 0 {
                call.write_u32(inside, spatial.cone_inside)?;
            }
            if outside != 0 {
                call.write_u32(outside, spatial.cone_outside)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "GetConeOrientation") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let spatial = spatial_of(host, object);
            if destination != 0 {
                write_vector(call, destination, spatial.cone_orientation)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "GetConeOutsideVolume") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let spatial = spatial_of(host, object);
            if destination != 0 {
                call.write_u32(destination, spatial.cone_outside_volume)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "GetMaxDistance" | "GetMinDistance") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let spatial = spatial_of(host, object);
            let value = if name == "GetMaxDistance" {
                spatial.max_distance
            } else {
                spatial.min_distance
            };
            if destination != 0 {
                call.write_u32(destination, value)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "GetMode") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let spatial = spatial_of(host, object);
            if destination != 0 {
                call.write_u32(destination, spatial.mode)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "GetPosition" | "GetVelocity") => {
            let object = call.arg(0)?;
            let destination = call.arg(1)?;
            let spatial = spatial_of(host, object);
            let vector = if name == "GetPosition" {
                spatial.position
            } else {
                spatial.velocity
            };
            if destination != 0 {
                write_vector(call, destination, vector)?;
            }
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetConeAngles") => {
            let object = call.arg(0)?;
            let inside = call.arg(1)?;
            let outside = call.arg(2)?;
            let spatial = spatial_of(host, object);
            host.audio.spatial.insert(
                object,
                Spatial {
                    cone_inside: inside,
                    cone_outside: outside,
                    ..spatial
                },
            );
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetConeOrientation") => {
            let object = call.arg(0)?;
            let vector = [call.arg(1)?, call.arg(2)?, call.arg(3)?];
            let spatial = spatial_of(host, object);
            host.audio.spatial.insert(
                object,
                Spatial {
                    cone_orientation: vector,
                    ..spatial
                },
            );
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetConeOutsideVolume") => {
            let object = call.arg(0)?;
            let volume = call.arg(1)?;
            let spatial = spatial_of(host, object);
            host.audio.spatial.insert(
                object,
                Spatial {
                    cone_outside_volume: volume,
                    ..spatial
                },
            );
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetMaxDistance" | "SetMinDistance") => {
            let object = call.arg(0)?;
            let distance = call.arg(1)?;
            let spatial = spatial_of(host, object);
            let updated = if name == "SetMaxDistance" {
                Spatial {
                    max_distance: distance,
                    ..spatial
                }
            } else {
                Spatial {
                    min_distance: distance,
                    ..spatial
                }
            };
            host.audio.spatial.insert(object, updated);
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetMode") => {
            let object = call.arg(0)?;
            let mode = call.arg(1)?;
            let spatial = spatial_of(host, object);
            host.audio.spatial.insert(
                object,
                Spatial {
                    mode,
                    ..spatial
                },
            );
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetPosition") => {
            let object = call.arg(0)?;
            let vector = [call.arg(1)?, call.arg(2)?, call.arg(3)?];
            let spatial = spatial_of(host, object);
            host.audio.spatial.insert(
                object,
                Spatial {
                    position: vector,
                    ..spatial
                },
            );
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND_3D_BUFFER, "SetVelocity") => {
            let object = call.arg(0)?;
            let vector = [call.arg(1)?, call.arg(2)?, call.arg(3)?];
            let spatial = spatial_of(host, object);
            host.audio.spatial.insert(
                object,
                Spatial {
                    velocity: vector,
                    ..spatial
                },
            );
            Ok(Return(DS_OK))
        }

        // ---- IDirectSound -------------------------------------------------
        (thunks::DSOUND, "CreateSoundBuffer") => create_sound_buffer(host, call),
        (thunks::DSOUND, "DuplicateSoundBuffer") => duplicate_sound_buffer(host, call),
        (thunks::DSOUND, "SetCooperativeLevel") => {
            host.audio.cooperative_level = call.arg(2)?;
            Ok(Return(DS_OK))
        }
        (thunks::DSOUND, "GetCaps") => {
            let caps = call.arg(1)?;
            if caps != 0 {
                call.zero(caps, 96)?;
                // `DSDCAPS`: the size, then what this device can mix. A page
                // mixes 8- and 16-bit, mono and stereo at any rate, so saying so
                // is the truth -- and a game that only asks the device what it
                // can do before enabling its music decides on this answer.
                call.write_u32(caps, 96)?;
                call.write_u32(
                    caps + 4,
                    DSCAPS_PRIMARYMONO
                        | DSCAPS_PRIMARYSTEREO
                        | DSCAPS_PRIMARY8BIT
                        | DSCAPS_PRIMARY16BIT
                        | DSCAPS_SECONDARYMONO
                        | DSCAPS_SECONDARYSTEREO
                        | DSCAPS_SECONDARY8BIT
                        | DSCAPS_SECONDARY16BIT,
                )?;
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
        // The DX8 device answers whether the driver is certified: every normal
        // driver is, and a guest that is told otherwise turns features off.
        (thunks::DSOUND, "VerifyCertification") => {
            let certified = call.arg(1)?;
            if certified != 0 {
                call.write_u32(certified, DS_CERTIFIED)?;
            }
            Ok(Return(DS_OK))
        }

        // ---- IDirectSoundBuffer -------------------------------------------
        (thunks::DSOUND_BUFFER, "Lock") => lock(host, call),
        (thunks::DSOUND_BUFFER, "Unlock") => unlock(host, call),
        (thunks::DSOUND_BUFFER, "Play") => play(host, call),
        (thunks::DSOUND_BUFFER, "Stop") => {
            let this = call.arg(0)?;
            // The play cursor stays where the sound got to, and `Play` resumes
            // from it, so it has to be turned into a frozen offset here rather
            // than left to go on advancing while nothing plays.
            let position = position_of(host, this);
            let now = host.now_ms();
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.start_bytes = position;
                buffer.started_ms = now;
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
        // A buffer's samples are guest memory nothing takes away, so there is
        // nothing lost to restore.
        (thunks::DSOUND_BUFFER, "Restore") => Ok(Return(DS_OK)),
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
            let mut start = position;
            if let Some(buffer) = host.audio.buffers.get_mut(&this) {
                buffer.start_bytes = position.min(buffer.size);
                buffer.started_ms = now;
                start = buffer.start_bytes;
            }
            host.audio.events.push(AudioEvent {
                kind: EVENT_SEEK,
                voice: this,
                bytes: start,
                ..AudioEvent::default()
            });
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
            // The flags come back as the guest asked for them, which is what a
            // real driver reports: a buffer created with control flags has them.
            call.write_u32(
                caps + 4,
                if buffer.primary {
                    DSBCAPS_PRIMARYBUFFER
                } else {
                    buffer.caps
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
            let written = call.arg(3)?;
            if let Some(buffer) = host.audio.buffers.get(&this) {
                let mut wrote = 0;
                if destination != 0 {
                    wrote = if bytes == 0 { WFX_BYTES } else { bytes.min(WFX_BYTES) };
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
                    call.write(destination, &format[..wrote as usize])?;
                }
                // The size a guest asked for with a NULL format pointer, or the
                // bytes a write filled, are what it sizes its next query from.
                if written != 0 {
                    call.write_u32(written, if destination == 0 { WFX_BYTES } else { wrote })?;
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
    };

    // A refusal is the one thing the call trail cannot show any other way: the
    // guest is told `DSERR_INVALIDPARAM` and does something else with its
    // voice, and from the outside that is indistinguishable from a guest that
    // simply never asked. `DuplicateSoundBuffer` is where it matters -- a
    // duplicate that fails is a sound the guest thinks it owns and cannot stop.
    if let Ok(Return(code)) = &outcome {
        if code & 0x8000_0000 != 0 && (LOG_AUDIO.load(Ordering::Relaxed) || std::env::var("SHIMKIT_LOG_AUDIO").is_ok())
        {
            host.notice(format!(
                "audio {name} refused {code:#010x} this={:#010x}",
                call.arg(0).unwrap_or(0)
            ));
        }
    }
    outcome
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
        // A duplicated buffer shares the original's bytes, so the block goes back
        // to the arena only when the last buffer over it is released: the guest is
        // free to release either one first, and the survivor must still lock.
        let shared = host
            .audio
            .buffers
            .values()
            .any(|other| other.data == buffer.data);
        if buffer.data != 0 && !shared {
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

/// `IDirectSound::DuplicateSoundBuffer(original, &duplicate)`: a second buffer
/// over the same samples.
///
/// DirectSound's duplicate shares the original's memory, and so does this: the
/// copy gets the same guest address, so `Lock` hands out the same bytes and a
/// write through either is seen by both. What it does not share is playback --
/// a play cursor, a volume and a frequency of its own is the whole point, which
/// is how a game plays one sample as two voices -- so those start from the
/// original's settings but nothing about it is already sounding.
///
/// The samples are one arena allocation seen twice, so [`release`] keeps the
/// block until both objects are gone.
fn duplicate_sound_buffer(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
    let source = call.arg(1)?;
    let destination = call.arg(2)?;
    if host.audio.device != Some(this) {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    if destination == 0 {
        return Ok(Return(DSERR_INVALIDPARAM));
    }
    let Some(original) = host.audio.buffers.get(&source) else {
        return Ok(Return(DSERR_INVALIDPARAM));
    };
    // Copied out field by field: `new_object` needs the host mutably, so the
    // borrow of the original has to end here. The samples are the one thing
    // cloned, because `pcm` is the copy's own snapshot of them.
    let (primary, caps, size, data, format, volume, pan, frequency, looping, pcm) = (
        original.primary,
        original.caps,
        original.size,
        original.data,
        original.format,
        original.volume,
        original.pan,
        original.frequency,
        original.looping,
        original.pcm.clone(),
    );
    let object = thunks::new_object(call.memory, host, thunks::DSOUND_BUFFER)?;
    let duplicate = SoundBuffer {
        refs: 1,
        primary,
        caps,
        size,
        data,
        format,
        volume,
        pan,
        frequency,
        playing: false,
        looping,
        started_ms: 0,
        start_bytes: 0,
        pcm,
    };
    host.audio.buffers.insert(object, duplicate);
    call.write_u32(destination, object)?;
    Ok(Return(DS_OK))
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
    // Behind the audio log: every stream a game opens reports one of these, and
    // the sizes are only interesting to whoever is chasing a silence.
    if std::env::var("SHIMKIT_LOG_AUDIO").is_ok() && host.notices.len() < 600 {
        // The caller's address, so a stream of buffer creations names the routine
        // that asked for them rather than only their sizes.
        let caller = call.memory.read_u32(call.args.wrapping_sub(4)).unwrap_or(0);
        host.notice(format!(
            "audio buffer {bytes} bytes, flags {:#x}, format {:#x} at {:#010x}",
            call.memory.read_u32(desc + DESC_FLAGS).unwrap_or(0),
            format_address,
            caller
        ));
    }

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
        caps: flags,
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
        if std::env::var("SHIMKIT_LOG_AUDIO").is_ok() && host.notices.len() < 600 {
            host.notice("audio Lock on the primary buffer was refused".to_string());
        }
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

/// `Play`: start at the play cursor.
///
/// It does not rewind. A buffer that is already playing keeps its position and
/// only takes the new loop flag, which is what a guest that sustains a looping
/// waveform and retunes it relies on: Organya plays a note by setting the
/// frequency and calling `Play` again, and rewinding there would cut the wave
/// at every note, heard as a click per note. A guest that wants the start of
/// the buffer says so, with `Stop` and `SetCurrentPosition`.
fn play(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let this = call.arg(0)?;
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
        // A one-shot that has already run out is a buffer to start again, not a
        // waveform to retune: `playing` alone stays true because only a status
        // query clears it, so the sound a guest asks for twice would be silent
        // the second time.
        let finished = !buffer.looping
            && now.saturating_sub(buffer.started_ms) >= buffer.duration_ms();
        let resuming = !buffer.playing || finished;
        buffer.playing = true;
        buffer.looping = flags & DSBPLAY_LOOPING != 0;
        if resuming {
            // Stopped: the cursor was frozen at `start_bytes`, and playing goes
            // on from there. Still playing: leave the cursor alone.
            buffer.started_ms = now;
        }
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
        // Both bits, the way a device reports it: a looping buffer is a playing
        // buffer that does not end. Answering `LOOPING` alone is a value no
        // device produces, and a guest that tests `DSBSTATUS_PLAYING` -- which
        // is how Hercules decides a voice is finished -- reads it as stopped,
        // forgets the voice, and leaves the loop running with nobody able to
        // stop it. See the module note.
        return DSBSTATUS_PLAYING | DSBSTATUS_LOOPING;
    }
    let elapsed = now.saturating_sub(buffer.started_ms);
    if elapsed < buffer.duration_ms() {
        DSBSTATUS_PLAYING
    } else {
        buffer.playing = false;
        0
    }
}

pub(crate) fn position_of(host: &Host, this: u32) -> u32 {
    let Some(buffer) = host.audio.buffers.get(&this) else {
        return 0;
    };
    if !buffer.playing || buffer.format.rate == 0 {
        return buffer.start_bytes;
    }
    let elapsed = host.now_ms().saturating_sub(buffer.started_ms);
    let bytes = (elapsed as u128)
        .saturating_mul(buffer.play_rate() as u128)
        .saturating_mul(buffer.frame_bytes() as u128)
        / 1000;
    let mut position = (buffer.start_bytes as u128).saturating_add(bytes);
    if buffer.size != 0 {
        // Only a looping buffer wraps. A one-shot that has run out reports the
        // end of its buffer: wrapping it reports a small position instead, and a
        // mixer that follows the playhead then starts the sound again from there.
        // Every one-shot then plays forever, and a minute of them mixes into the
        // hash that never recovers.
        if buffer.looping {
            position %= buffer.size as u128;
        } else {
            position = position.min(u128::from(buffer.size));
        }
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
    fn a_duplicated_buffer_shares_the_original_samples() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        let buffer = create_secondary(&mut host, &mut memory, device, 8, 1, 22_050);
        let out = STACK_BASE + 0x7600;
        let mut call = invoke(&mut memory, &[device, buffer, out]);
        assert_eq!(
            serve(&mut host, &mut call, thunks::DSOUND, "DuplicateSoundBuffer").unwrap(),
            Return(DS_OK)
        );
        let duplicate = call.memory.read_u32(out).unwrap();
        assert_ne!(duplicate, buffer, "the copy is an object of its own");
        let samples = host.audio.buffers[&buffer].data;
        assert_eq!(host.audio.buffers[&duplicate].data, samples);
        assert_eq!(host.audio.buffers[&duplicate].size, host.audio.buffers[&buffer].size);
        assert!(!host.audio.buffers[&duplicate].playing, "nothing is already sounding");

        // Releasing the original must not hand its bytes back to the arena: the
        // duplicate is still playing them.
        let mut call = invoke(&mut memory, &[buffer]);
        serve(&mut host, &mut call, thunks::DSOUND_BUFFER, "Release").unwrap();
        assert!(host.audio.buffers.contains_key(&duplicate));
        let pointer = STACK_BASE + 0x7400;
        let length = STACK_BASE + 0x7404;
        let mut call = invoke(&mut memory, &[duplicate, 0, 8, pointer, length, 0, 0, 0]);
        assert_eq!(
            serve(&mut host, &mut call, thunks::DSOUND_BUFFER, "Lock").unwrap(),
            Return(DS_OK)
        );
        assert_eq!(call.memory.read_u32(pointer).unwrap(), samples);
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
        // A looping buffer reports both bits, as a device does. Hercules tests
        // bit 0 to decide whether a voice is still busy, so `4` alone reads as
        // "finished" and leaves the loop sounding with no owner.
        let mut status = invoke(&mut memory, &[buffer, 0]);
        let answered = serve(
            &mut host,
            &mut status,
            thunks::DSOUND_BUFFER,
            "GetStatus",
        )
        .unwrap();
        assert_eq!(answered, Return(DSBSTATUS_PLAYING | DSBSTATUS_LOOPING));
        assert_eq!(
            answered,
            Return(5),
            "PLAYING is bit 0 and LOOPING is bit 2, which is what a device answers"
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

    /// A second `Play` is not a rewind: Organya sustains a looping waveform and
    /// retunes it, and starting it over at every note is a click per note.
    #[test]
    fn play_leaves_a_playing_buffer_where_it_is() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        host.set_clock(1_000);
        let device = open(&mut host, &mut memory);
        // 22050 frames at 22050 Hz is one second, so a millisecond is 22 bytes.
        let buffer = create_secondary(&mut host, &mut memory, device, 44_100, 1, 22_050);
        let position = |host: &mut Host, memory: &mut GuestMemory| {
            let out = STACK_BASE + 0x7500;
            let mut call = invoke(memory, &[buffer, out, 0]);
            serve(host, &mut call, thunks::DSOUND_BUFFER, "GetCurrentPosition").unwrap();
            call.memory.read_u32(out).unwrap()
        };
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 1]);
        serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap();
        host.set_clock(1_500);
        assert_eq!(position(&mut host, &mut memory), 11_025);
        // The same call again, half a second in, keeps the cursor there.
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 1]);
        serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap();
        assert_eq!(position(&mut host, &mut memory), 11_025);
        // Stop and Play resumes from where it stopped rather than from zero.
        let mut stop = invoke(&mut memory, &[buffer]);
        serve(&mut host, &mut stop, thunks::DSOUND_BUFFER, "Stop").unwrap();
        host.set_clock(3_000);
        let mut play = invoke(&mut memory, &[buffer, 0, 0, 1]);
        serve(&mut host, &mut play, thunks::DSOUND_BUFFER, "Play").unwrap();
        assert_eq!(position(&mut host, &mut memory), 11_025);
        // SetCurrentPosition is how a guest asks for the start, and the play
        // after it starts there.
        let mut seek = invoke(&mut memory, &[buffer, 0]);
        serve(&mut host, &mut seek, thunks::DSOUND_BUFFER, "SetCurrentPosition").unwrap();
        assert_eq!(position(&mut host, &mut memory), 0);
        // The page keeps its own playhead, so a rewind has to reach it: an
        // event carrying the byte the guest asked for, or the mixer keeps
        // playing from where it was and a drum restarted this way is silent.
        let event = host.audio.events().last().copied().unwrap();
        assert_eq!(event.kind, EVENT_SEEK);
        assert_eq!(event.voice, buffer);
        assert_eq!(event.bytes, 0);
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
    fn a_voice_names_the_memory_its_samples_live_in() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let device = open(&mut host, &mut memory);
        let buffer = create_secondary(&mut host, &mut memory, device, 8, 1, 22_050);
        let (address, length) = host.audio.samples(buffer).expect("a voice has samples");
        assert_eq!(length, 8);
        assert_ne!(address, 0);
        // The guest writes into that memory, so what the page reads is the sound
        // as it stands, not a copy taken when `Play` was called.
        memory.write_bytes(address, &[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
        let bytes = memory.read_bytes(address, 8).unwrap().to_vec();
        assert_eq!(bytes, vec![1, 2, 3, 4, 5, 6, 7, 8]);

        // A released voice has had its memory handed back, so the page has to
        // fall back on the copy the host kept.
        let mut release = invoke(&mut memory, &[buffer]);
        assert_eq!(
            serve(&mut host, &mut release, thunks::DSOUND_BUFFER, "Release").unwrap(),
            Return(0)
        );
        assert!(host.audio.samples(buffer).is_none());
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
