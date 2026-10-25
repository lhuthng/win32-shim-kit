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

pub const SHIMS: &[Binding] = &[
    Binding { dll: WINMM, name: "auxGetDevCapsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "auxGetNumDevs", arg_bytes: 0, shim: no_devices },
    Binding { dll: WINMM, name: "auxGetVolume", arg_bytes: 8, shim: no_driver },
    Binding { dll: WINMM, name: "joyGetDevCapsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "joyGetPosEx", arg_bytes: 8, shim: joystick_unplugged },
    Binding { dll: WINMM, name: "mciSendCommandA", arg_bytes: 16, shim: mci_send_command },
    Binding { dll: WINMM, name: "mixerGetControlDetailsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerGetDevCapsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerGetLineControlsA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerGetLineInfoA", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "mixerSetControlDetails", arg_bytes: 12, shim: no_driver },
    Binding { dll: WINMM, name: "timeBeginPeriod", arg_bytes: 4, shim: time_begin_period },
    Binding { dll: WINMM, name: "timeGetDevCaps", arg_bytes: 8, shim: time_get_dev_caps },
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
        let mut headless = Host::new();
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
