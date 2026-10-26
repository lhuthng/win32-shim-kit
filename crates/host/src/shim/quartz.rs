//! A `DirectShow` filter graph, as far as the movie an opening sequence plays
//! needs one.
//!
//! A game that plays a video asks `CoCreateInstance` for `CLSID_FilterGraph` and
//! treats an interface it cannot get as a fatal start-up problem: the graph is
//! created before the movie's name is even looked at, so refusing it shows an
//! error box and stops the sequence. The graph itself is not the interesting
//! part -- the file is -- so this host creates the graph and answers it with a
//! `VFW_E_NOT_FOUND` when the movie is rendered. That is what a machine with the
//! codec but without the file says, and it is the answer a game that ships no
//! movie has to survive anyway.
//!
//! Everything else a graph answers is `S_OK` with no state behind it: a graph
//! that renders nothing runs nothing, and reporting that it is running is what
//! the caller sees when it asks. The methods are answered one table per
//! interface, because the vtable slot numbers differ between them and a slot
//! number alone does not say which interface was called.

use crate::thunks::{self, Method};
use crate::shim::{Action, Call, Host};
use crate::Result;

use Action::Return;

/// Interface kinds, reserved in [`thunks`]'s encoding.
pub const GRAPH: u8 = 8;
pub const CONTROL: u8 = 9;
pub const EVENT: u8 = 10;
pub const POSITION: u8 = 11;
pub const VIDEO_WINDOW: u8 = 12;
pub const BASIC_AUDIO: u8 = 13;

const S_OK: u32 = 0;
/// The object exists but does not implement the requested interface.
const E_NOINTERFACE: u32 = 0x8000_4002;
/// `VFW_E_NOT_FOUND`: no file matches the name that was rendered.
const VFW_E_NOT_FOUND: u32 = 0x8004_0216;
/// `E_ABORT`: the call gave up waiting, which is what an empty event queue says.
const E_ABORT: u32 = 0x8000_4004;

/// A GUID in the form a call site passes it: four words read in the order the
/// class id and interface ids are stored, which is the canonical layout rather
/// than four hexadecimal fields of one number.
const fn guid(data1: u32, data2: u16, data3: u16, tail: [u8; 8]) -> [u32; 4] {
    [
        data1,
        data2 as u32 | (data3 as u32) << 16,
        u32::from_le_bytes([tail[0], tail[1], tail[2], tail[3]]),
        u32::from_le_bytes([tail[4], tail[5], tail[6], tail[7]]),
    ]
}

/// The last eight bytes of `CLSID_FilterGraph`.
const FILTER_GRAPH_TAIL: [u8; 8] = [0x9F, 0x53, 0x00, 0x20, 0xAF, 0x0B, 0xA7, 0x70];
/// The last eight bytes of every interface id below, `B03A-0020AF0BA770`: the
/// class and its interfaces were registered together but not under one tail.
const DIRECTSHOW_TAIL: [u8; 8] = [0xB0, 0x3A, 0x00, 0x20, 0xAF, 0x0B, 0xA7, 0x70];

/// `CLSID_FilterGraph`, the class a program creates before it renders anything.
const CLSID_FILTER_GRAPH: [u32; 4] =
    guid(0xE436_EBB3, 0x524F, 0x11CE, FILTER_GRAPH_TAIL);

/// `IID_IGraphBuilder`.
const IID_GRAPH_BUILDER: [u32; 4] = guid(0x56A8_68A9, 0x0AD4, 0x11CE, DIRECTSHOW_TAIL);
/// `IID_IMediaControl`.
const IID_MEDIA_CONTROL: [u32; 4] = guid(0x56A8_68B1, 0x0AD4, 0x11CE, DIRECTSHOW_TAIL);
/// `IID_IMediaEvent`.
const IID_MEDIA_EVENT: [u32; 4] = guid(0x56A8_68B6, 0x0AD4, 0x11CE, DIRECTSHOW_TAIL);
/// `IID_IMediaPosition`.
const IID_MEDIA_POSITION: [u32; 4] = guid(0x56A8_68B2, 0x0AD4, 0x11CE, DIRECTSHOW_TAIL);
/// `IID_IVideoWindow`.
const IID_VIDEO_WINDOW: [u32; 4] = guid(0x56A8_68B4, 0x0AD4, 0x11CE, DIRECTSHOW_TAIL);
/// `IID_IBasicAudio`.
const IID_BASIC_AUDIO: [u32; 4] = guid(0x56A8_68B3, 0x0AD4, 0x11CE, DIRECTSHOW_TAIL);

/// `State_Running`, the state a graph that has nothing to render reports.
const STATE_RUNNING: u32 = 2;

/// `IGraphBuilder`, at the offsets the graph object puts its methods at.
///
/// `RenderFile` is entry 13: the same ten-method gap the other interfaces have
/// sits before the builder's own methods, and this is the entry a program calls
/// into quartz.dll at. Entries this host has not seen called are gaps, so a
/// program that uses one stops on a message naming the offset.
const GRAPH_METHODS: &[Option<Method>] = &[
    Method::new("QueryInterface", 12),
    Method::new("AddRef", 4),
    Method::new("Release", 4),
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    None,
    Method::new("RenderFile", 12),
];

/// `IMediaControl`, at the same offsets: `Run` is entry 7.
const CONTROL_METHODS: &[Option<Method>] = &[
    Method::new("QueryInterface", 12),
    Method::new("AddRef", 4),
    Method::new("Release", 4),
    None,
    None,
    None,
    None,
    Method::new("Run", 4),
    Method::new("Stop", 4),
    Method::new("Pause", 4),
    Method::new("GetState", 12),
    Method::new("GetGraph", 8),
    Method::new("RenderFile", 12),
];

/// `IMediaEvent`, at the offsets its object puts the methods at.
///
/// DirectShow's filter graph is one C++ object, and a C++ vtable holds a class's
/// own virtual methods in declaration order -- so `GetEvent` sits at entry 8
/// rather than entry 3, past five of the class's other methods that no interface
/// names. The offsets are therefore the object's, not the interface's, and this
/// is the layout a program compiled against quartz.dll calls into. Entries this
/// host has not seen called are gaps, so a program that uses one stops on a
/// message naming the offset instead of reaching a method with a wrong stack.
const EVENT_METHODS: &[Option<Method>] = &[
    Method::new("QueryInterface", 12),
    Method::new("AddRef", 4),
    Method::new("Release", 4),
    None,
    None,
    None,
    None,
    None,
    Method::new("GetEvent", 20),
    Method::new("WaitForCompletion", 12),
    Method::new("CancelDefaultHandling", 8),
    Method::new("RestoreDefaultHandling", 8),
    Method::new("FreeEventParams", 16),
    Method::new("SetNotifyWindow", 16),
    Method::new("SetNotifyFlags", 8),
    Method::new("GetNotifyFlags", 8),
];

/// `IMediaPosition`, at the same object's offsets: its methods start at entry 7,
/// four past `Release`. A graph with no file has no position, so every getter
/// answers zero and every setter is accepted and dropped.
const POSITION_METHODS: &[Option<Method>] = &[
    Method::new("QueryInterface", 12),
    Method::new("AddRef", 4),
    Method::new("Release", 4),
    None,
    None,
    None,
    None,
    Method::new("get_Duration", 8),
    Method::new("put_CurrentPosition", 12),
    Method::new("get_CurrentPosition", 8),
    Method::new("get_StopTime", 8),
    Method::new("put_StopTime", 12),
    Method::new("get_PrerollTime", 8),
    Method::new("put_PrerollTime", 12),
    Method::new("get_Rate", 8),
    Method::new("put_Rate", 12),
    Method::new("CanSeekForward", 8),
    Method::new("CanSeekBackward", 8),
];

/// `IVideoWindow`, in vtable order. The window the movie would draw into is the
/// one it is given; nothing is drawn, so setting it is all there is.
const VIDEO_WINDOW_METHODS: &[Option<Method>] = &[
    Method::new("QueryInterface", 12),
    Method::new("AddRef", 4),
    Method::new("Release", 4),
    None,
    None,
    None,
    None,
    None,
    Method::new("put_Caption", 8),
    Method::new("get_Caption", 8),
    Method::new("put_WindowStyle", 8),
    Method::new("get_WindowStyle", 8),
    Method::new("put_WindowStyleEx", 8),
    Method::new("get_WindowStyleEx", 8),
    Method::new("put_AutoShow", 8),
    Method::new("get_AutoShow", 8),
    Method::new("put_WindowState", 8),
    Method::new("get_WindowState", 8),
    Method::new("put_BackgroundPalette", 8),
    Method::new("get_BackgroundPalette", 8),
    Method::new("put_Visible", 8),
    Method::new("get_Visible", 8),
    Method::new("put_Left", 8),
    Method::new("get_Left", 8),
    Method::new("put_Width", 8),
    Method::new("get_Width", 8),
    Method::new("put_Top", 8),
    Method::new("get_Top", 8),
    Method::new("put_Height", 8),
    Method::new("get_Height", 8),
    Method::new("put_Owner", 8),
    Method::new("get_Owner", 8),
    Method::new("put_MessageDrain", 8),
    Method::new("get_MessageDrain", 8),
    Method::new("get_BorderColor", 8),
    Method::new("put_BorderColor", 8),
    Method::new("get_FullScreenMode", 8),
    Method::new("put_FullScreenMode", 8),
    Method::new("SetWindowForeground", 8),
    Method::new("NotifyOwnerMessage", 20),
    Method::new("SetWindowPosition", 16),
    Method::new("GetWindowPosition", 12),
    Method::new("GetMaxIdealImageSize", 12),
    Method::new("GetMinIdealImageSize", 12),
    Method::new("GetRestorePosition", 12),
    Method::new("HideCursor", 8),
    Method::new("IsCursorHidden", 8),
];

/// `IBasicAudio`, at the same offsets: volume is entry 7 here, one earlier than
/// the event queue's methods, because the two classes declare a different number
/// of methods of their own before the interface's.
const BASIC_AUDIO_METHODS: &[Option<Method>] = &[
    Method::new("QueryInterface", 12),
    Method::new("AddRef", 4),
    Method::new("Release", 4),
    None,
    None,
    None,
    None,
    Method::new("put_Volume", 8),
    Method::new("get_Volume", 8),
    Method::new("put_Balance", 8),
    Method::new("get_Balance", 8),
];

/// The filter graph this session created, if any.
///
/// One graph is enough for the interface surface here: a game creates it once and
/// asks its controls and its event queue for the graph back.
#[derive(Default)]
pub struct Quartz {
    /// The `IGraphBuilder` object `CoCreateInstance` handed out.
    pub graph: Option<u32>,
}

/// The method table of one interface kind, for [`thunks`].
pub(crate) fn methods(kind: u8) -> Option<&'static [Option<Method>]> {
    match kind {
        GRAPH => Some(GRAPH_METHODS),
        CONTROL => Some(CONTROL_METHODS),
        EVENT => Some(EVENT_METHODS),
        POSITION => Some(POSITION_METHODS),
        VIDEO_WINDOW => Some(VIDEO_WINDOW_METHODS),
        BASIC_AUDIO => Some(BASIC_AUDIO_METHODS),
        _ => None,
    }
}

/// The interface kind an IID names, if this host implements it.
fn kind_of(iid: [u32; 4]) -> Option<u8> {
    match iid {
        IID_GRAPH_BUILDER => Some(GRAPH),
        IID_MEDIA_CONTROL => Some(CONTROL),
        IID_MEDIA_EVENT => Some(EVENT),
        IID_MEDIA_POSITION => Some(POSITION),
        IID_VIDEO_WINDOW => Some(VIDEO_WINDOW),
        IID_BASIC_AUDIO => Some(BASIC_AUDIO),
        _ => None,
    }
}

/// The GUID at `address`, as four dwords in the order it is stored.
fn read_guid(call: &Call<'_>, address: u32) -> Result<[u32; 4]> {
    let mut words = [0_u32; 4];
    for (index, word) in words.iter_mut().enumerate() {
        *word = call.memory.read_u32(address.wrapping_add(index as u32 * 4))?;
    }
    Ok(words)
}

/// `CoCreateInstance` for `CLSID_FilterGraph`, or `None` when the class is not
/// one this module knows.
///
/// The caller falls through to `E_NOINTERFACE` for `None`, so a class that is
/// not a filter graph is answered exactly as it was before this module existed.
pub fn create_instance(host: &mut Host, call: &mut Call<'_>) -> Result<Option<Action>> {
    let class = read_guid(call, call.arg(0)?)?;
    if class != CLSID_FILTER_GRAPH {
        return Ok(None);
    }
    let wanted = read_guid(call, call.arg(3)?)?;
    let Some(kind) = kind_of(wanted) else {
        let destination = call.arg(4)?;
        if destination != 0 {
            call.write_u32(destination, 0)?;
        }
        return Ok(Some(Return(E_NOINTERFACE)));
    };
    let object = thunks::new_object(call.memory, host, kind)?;
    if kind == GRAPH {
        host.quartz.graph = Some(object);
    }
    let destination = call.arg(4)?;
    if destination != 0 {
        call.write_u32(destination, object)?;
    }
    Ok(Some(Return(S_OK)))
}

/// Answers one method of a graph, its controls, its event queue, its position or
/// its video window.
pub fn serve(host: &mut Host, call: &mut Call<'_>, _kind: u8, name: &str) -> Result<Action> {
    match name {
        "AddRef" | "Release" => Ok(Return(1)),
        "QueryInterface" => {
            let wanted = read_guid(call, call.arg(1)?)?;
            let destination = call.arg(2)?;
            let Some(wanted_kind) = kind_of(wanted) else {
                return Ok(Return(E_NOINTERFACE));
            };
            let object = thunks::new_object(call.memory, host, wanted_kind)?;
            if destination != 0 {
                call.write_u32(destination, object)?;
            }
            Ok(Return(S_OK))
        }
        // The graph has no file to render, which is the answer a machine with the
        // codec but without the movie gives, and the one a game that ships no
        // movie already handles.
        // The file is opened if this host has it, and the graph says so, but there
        // is no decoder behind the graph: what a caller plays through it is silent.
        // A missing file is the one answer that is real, and a caller already
        // handles it -- a game whose intro movie is not installed ships without it.
        "RenderFile" => {
            let path = call.wstr(call.arg(1)?).unwrap_or_default();
            if host.files.contents(&path).is_none() {
                host.notice(format!(
                    "DirectShow: {path:?} is not in the bundle; the graph answers VFW_E_NOT_FOUND"
                ));
                return Ok(Return(VFW_E_NOT_FOUND));
            }
            host.notice(format!(
                "DirectShow: {path:?} is rendered, but this host has no decoder, so it is silent"
            ));
            Ok(Return(S_OK))
        }
        "Run" | "Stop" | "Pause" | "FindFilterByName" | "AddFilter"
        | "RemoveFilter" | "ConnectDirect" | "Reconnect" | "Disconnect" | "EnumFilters"
        | "AddSourceFilter" | "SetFiltergraph" | "SetDefaultSyncSource"
        | "CancelDefaultHandling" | "RestoreDefaultHandling" | "put_WindowStyle"
        | "put_WindowStyleEx" | "put_AutoShow" | "put_WindowState"
        | "put_BackgroundPalette" | "put_Visible" | "put_Left" | "put_Width" | "put_Top"
        | "put_Height" | "put_Owner" | "put_MessageDrain" | "put_BorderColor"
        | "put_FullScreenMode" | "SetWindowForeground" | "NotifyOwnerMessage"
        | "SetWindowPosition" | "HideCursor" | "put_Volume" | "put_Balance"
        | "put_CurrentPosition" | "put_StopTime" | "put_PrerollTime" | "put_Rate" => {
            Ok(Return(S_OK))
        }
        // The graph the control belongs to, which is the object this host handed
        // out for `CLSID_FilterGraph`: a caller that asks for it gets the same one
        // back and can build on it.
        "GetGraph" => {
            let destination = call.arg(1)?;
            if destination != 0 {
                let graph = host.quartz.graph.unwrap_or(call.arg(0)?);
                call.write_u32(destination, graph)?;
            }
            Ok(Return(S_OK))
        }
        "GetState" => {
            let state = call.arg(1)?;
            if state != 0 {
                call.write_u32(state, STATE_RUNNING)?;
            }
            Ok(Return(S_OK))
        }
        // An event queue that never receives anything. `GetEvent` says so with
        // `E_ABORT`, which is the "nothing arrived in the time asked for" answer a
        // caller already handles; `WaitForCompletion` reports completion, since
        // there is nothing playing to wait for.
        "GetEvent" => Ok(Return(E_ABORT)),
        "WaitForCompletion" => {
            let code = call.arg(2)?;
            if code != 0 {
                call.write_u32(code, 0)?;
            }
            Ok(Return(S_OK))
        }
        // No window receives direct-show messages: the graph owns no event source
        // to forward, which is what a graph with no filter in it has.
        "SetNotifyWindow" | "SetNotifyFlags" => Ok(Return(S_OK)),
        // A rate of 1.0 is the neutral value a caller multiplies by; everything
        // else this graph can be asked about is a number it does not have, which
        // is zero. `CanSeek*` is a `BOOL`, so it is zero too: no file, no seeking.
        "get_Rate" => {
            let rate = call.arg(1)?;
            if rate != 0 {
                call.write(rate, &1.0_f64.to_le_bytes())?;
            }
            Ok(Return(S_OK))
        }
        getter if getter.starts_with("get_") || getter.starts_with("CanSeek") => {
            let value = call.arg(1)?;
            if value != 0 {
                call.write_u32(value, 0)?;
            }
            Ok(Return(S_OK))
        }
        other => Err(format!(
            "{}::{other} has no shim: the graph exists but this method does not",
            thunks::kind_name(_kind)
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{GuestMemory, STACK_BASE};

    /// Where the argument area sits: inside the stack, which every mapped image
    /// has.
    const ARGS: u32 = STACK_BASE + 0x4000;

    fn machine() -> Option<(Host, GuestMemory)> {
        let image = crate::reference_image()?;
        Some((Host::new(), GuestMemory::load(image).expect("image maps")))
    }

    /// The bytes of a GUID in the order `CoCreateInstance` reads them.
    fn bytes(guid: [u32; 4]) -> Vec<u8> {
        guid.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    fn write_args(memory: &mut GuestMemory, values: &[u32]) {
        for (index, value) in values.iter().enumerate() {
            memory
                .write_u32(ARGS + index as u32 * 4, *value)
                .expect("the argument area is mapped");
        }
    }

    fn call<'a>(memory: &'a mut GuestMemory, args: u32) -> Call<'a> {
        Call {
            memory,
            args,
            arg_bytes: 20,
        }
    }

    #[test]
    fn the_filter_graph_class_is_created_for_the_interface_that_was_asked_for() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let class = ARGS + 0x100;
        let iid = ARGS + 0x200;
        let destination = ARGS + 0x300;
        memory.write_bytes(class, &bytes(CLSID_FILTER_GRAPH)).unwrap();
        memory.write_bytes(iid, &bytes(IID_MEDIA_CONTROL)).unwrap();
        write_args(&mut memory, &[class, 0, 1, iid, destination]);

        let action = create_instance(&mut host, &mut call(&mut memory, ARGS))
            .expect("the class is answered")
            .expect("the class is a filter graph");
        assert_eq!(action, Return(S_OK));
        let object = memory.read_u32(destination).unwrap();
        assert_ne!(object, 0);
        // The object points at the table for `IMediaControl`, whose first entry is
        // the `QueryInterface` thunk of that kind.
        let table = memory.read_u32(object).unwrap();
        assert_eq!(
            memory.read_u32(table).unwrap(),
            thunks::address(CONTROL, 0)
        );
    }

    #[test]
    fn another_class_is_left_to_the_caller() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let class = ARGS + 0x100;
        let iid = ARGS + 0x200;
        memory
            .write_bytes(class, &bytes([0x1234_5678, 0, 0, 0]))
            .unwrap();
        memory.write_bytes(iid, &bytes(IID_GRAPH_BUILDER)).unwrap();
        write_args(&mut memory, &[class, 0, 1, iid, ARGS + 0x300]);

        assert!(create_instance(&mut host, &mut call(&mut memory, ARGS))
            .expect("no class is an error")
            .is_none());
    }

    #[test]
    fn an_interface_the_graph_does_not_implement_is_refused() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let object = thunks::new_object(&mut memory, &mut host, GRAPH).unwrap();
        let iid = ARGS + 0x200;
        let destination = ARGS + 0x300;
        memory.write_bytes(iid, &bytes([0xDEAD_BEEF, 0, 0, 0])).unwrap();
        write_args(&mut memory, &[object, iid, destination, 0, 0]);

        let action = serve(&mut host, &mut call(&mut memory, ARGS), GRAPH, "QueryInterface")
            .expect("the method is answered");
        assert_eq!(action, Return(E_NOINTERFACE));
    }

    #[test]
    fn a_query_for_another_interface_hands_back_that_interface() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let object = thunks::new_object(&mut memory, &mut host, GRAPH).unwrap();
        let iid = ARGS + 0x200;
        let destination = ARGS + 0x300;
        memory.write_bytes(iid, &bytes(IID_MEDIA_EVENT)).unwrap();
        write_args(&mut memory, &[object, iid, destination, 0, 0]);

        let action = serve(&mut host, &mut call(&mut memory, ARGS), GRAPH, "QueryInterface")
            .expect("the method is answered");
        assert_eq!(action, Return(S_OK));
        let event = memory.read_u32(destination).unwrap();
        let table = memory.read_u32(event).unwrap();
        assert_eq!(memory.read_u32(table).unwrap(), thunks::address(EVENT, 0));
    }

    #[test]
    fn rendering_a_movie_that_is_not_there_says_so_and_names_it() {
        let Some((mut host, mut memory)) = machine() else {
            return;
        };
        let object = thunks::new_object(&mut memory, &mut host, GRAPH).unwrap();
        let name = ARGS + 0x200;
        // "intro.avi" in UTF-16, terminated.
        let text: Vec<u8> = "intro.avi\0"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        memory.write_bytes(name, &text).unwrap();
        write_args(&mut memory, &[object, name, 0, 0, 0]);

        let action = serve(&mut host, &mut call(&mut memory, ARGS), GRAPH, "RenderFile")
            .expect("the method is answered");
        assert_eq!(action, Return(VFW_E_NOT_FOUND));
        assert!(
            host.notices.iter().any(|notice| notice.contains("intro.avi")),
            "the notice names the file: {:?}",
            host.notices
        );
    }

    /// A GUID read as four words has to come back as the bytes a call site
    /// passes, or the class id silently never matches: this pins the layout
    /// against the literals `CoCreateInstance` is called with.
    #[test]
    fn the_class_and_interface_ids_are_stored_in_the_canonical_layout() {
        assert_eq!(
            bytes(CLSID_FILTER_GRAPH),
            [
                0xb3, 0xeb, 0x36, 0xe4, 0x4f, 0x52, 0xce, 0x11, 0x9f, 0x53, 0x00, 0x20, 0xaf,
                0x0b, 0xa7, 0x70
            ]
        );
        assert_eq!(
            bytes(IID_GRAPH_BUILDER),
            [
                0xa9, 0x68, 0xa8, 0x56, 0xd4, 0x0a, 0xce, 0x11, 0xb0, 0x3a, 0x00, 0x20, 0xaf,
                0x0b, 0xa7, 0x70
            ]
        );
    }

    #[test]
    fn every_interface_kind_has_a_table_whose_slots_are_bounded() {
        for kind in [GRAPH, CONTROL, EVENT, POSITION, VIDEO_WINDOW, BASIC_AUDIO] {
            let table = methods(kind).expect("the kind has a table");
            assert!(table.len() <= usize::from(thunks::SLOT_LIMIT));
            assert_eq!(table[0].as_ref().map(|m| m.name), Some("QueryInterface"));
            assert_eq!(table[1].as_ref().map(|m| m.name), Some("AddRef"));
            assert_eq!(table[2].as_ref().map(|m| m.name), Some("Release"));
        }
    }
}
