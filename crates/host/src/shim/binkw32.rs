//! `binkw32.dll`: RAD Game Tools' Bink video, which every cutscene goes through.
//!
//! The game links against it directly rather than loading it on demand, so its
//! imports are part of the image's import table even in a session that never
//! plays a movie. What the guest does with it is narrow: open a `.bik` file that
//! sits beside the executable, poll `_BinkWait` until a frame is due, ask
//! `_BinkDoFrame` to decode it, copy the decoded pixels into its own texture with
//! `_BinkCopyToBuffer`, and advance with `_BinkNextFrame` until the last one.
//!
//! # The movies are not decoded here
//!
//! Bink is a proprietary codec, and a decoder is not what this host is: it runs
//! the guest's own code and answers the platform calls around it. So `_BinkOpen`
//! answers `NULL` -- the same thing Windows does when the file is missing or the
//! DLL cannot allocate -- and records a notice naming the movie that was asked
//! for. A guest that checks its handle, as this one does, takes the "no movie
//! available" path and goes on to its menu, which is the honest behaviour: the
//! session has no picture to show for a cutscene, and a silent skip is better
//! than a fabricated one.
//!
//! The export names carry their stdcall decoration (`_BinkOpen@8`) because that
//! is exactly how the guest's import table spells them: unlike the system DLLs it
//! imports from, this one exports decorated names.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const BINKW32: &str = "binkw32.dll";

pub const SHIMS: &[Binding] = &[
    Binding { dll: BINKW32, name: "_BinkClose@4", arg_bytes: 4, shim: bink_close },
    Binding { dll: BINKW32, name: "_BinkCopyToBuffer@28", arg_bytes: 28, shim: bink_copy_to_buffer },
    Binding { dll: BINKW32, name: "_BinkDoFrame@4", arg_bytes: 4, shim: bink_do_frame },
    Binding { dll: BINKW32, name: "_BinkNextFrame@4", arg_bytes: 4, shim: bink_next_frame },
    Binding { dll: BINKW32, name: "_BinkOpen@8", arg_bytes: 8, shim: bink_open },
    Binding { dll: BINKW32, name: "_BinkOpenDirectSound@4", arg_bytes: 4, shim: bink_open_audio },
    Binding { dll: BINKW32, name: "_BinkSetSoundSystem@8", arg_bytes: 8, shim: bink_set_audio },
    Binding { dll: BINKW32, name: "_BinkWait@4", arg_bytes: 4, shim: bink_wait },
];

/// `_BinkOpen(name, flags)`: no handle, and a notice saying which movie the
/// guest wanted. Answering `NULL` is the documented failure, so the guest's own
/// error path runs instead of decoding pixels that do not exist.
fn bink_open(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let movie = call.cstr(0).unwrap_or_default();
    host.notice(format!(
        "bink: {movie} is not decoded; the guest skips this movie"
    ));
    Ok(Return(0))
}


/// `_BinkWait(handle)`: whether a frame is being decoded. Nothing ever is here,
/// so a guest that polls it is never held up.
fn bink_wait(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_BinkDoFrame(handle)`: decode the next frame. With no video there is nothing
/// to decode and the call still reports that a frame is ready.
fn bink_do_frame(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `_BinkNextFrame(handle)`: advance the playhead. The guest stops when
/// `_BinkWait` says the movie is over, which `_BinkOpen`'s `NULL` already made
/// certain, so this is a no-op that answers the success value.
fn bink_next_frame(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `_BinkCopyToBuffer(handle, dest, pitch, height, x, y, flags)`: hand the
/// decoded pixels to the guest. Only reachable with a handle `_BinkOpen` never
/// gave out, so nothing is written and the copy reports success.
fn bink_copy_to_buffer(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `_BinkClose(handle)`: release the movie. There is no handle to release.
fn bink_close(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `_BinkSetSoundSystem(system, param)`: name the audio backend Bink should mix
/// into. The guest passes DirectSound; this host's mixer is the page's, so the
/// choice is accepted and nothing is opened.
fn bink_set_audio(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(1))
}

/// `_BinkOpenDirectSound(handle)`: the DirectSound output a movie would mix
/// into. No movie is opened, so there is no output to report.
fn bink_open_audio(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}
