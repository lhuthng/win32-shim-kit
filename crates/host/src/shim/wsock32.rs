//! `WSOCK32.dll`: the WinSock 1.1 a page has no stack for.
//!
//! A game asks for the socket library even when it will never connect to
//! anything: the online menu is compiled in, its start-up runs before the menu
//! is chosen, and the imports are by ordinal so nothing can be skipped. The
//! answer here is the one a machine with the library installed and no network
//! gives: `WSAStartup` succeeds, so the program is not blocked at the door, and
//! every operation that would need a socket fails. What the game does with that
//! is its own business -- Little Fighter 2's mod reports that it cannot connect
//! and stays in single player, which is the mode this host can actually run.
//!
//! Two kinds of call are answered for real, because they are arithmetic rather
//! than networking: the byte-order swaps (`htons` and its three siblings) and
//! `inet_addr`, which turns `"127.0.0.1"` into the word a caller stores. A game
//! computes an address before it tries to connect, and answering those with a
//! failure would break a code path that never reaches the network anyway.
//!
//! `socket` is where the truth is told: it fails with `WSAENETDOWN`, and says so
//! once in the notices, because a program that silently stops at its online menu
//! looks like a shim that is missing rather than a machine with no network. The
//! calls that take a socket answer `WSAENOTSOCK` for the same reason: there is no
//! socket to hand back, so the handle a caller passes cannot be one.

use crate::shim::{Action, Binding, Call, Host};
use crate::Result;

use Action::Return;

const WSOCK32: &str = "WSOCK32.dll";

/// The value both `socket` and the failing calls answer with: `-1` read as an
/// unsigned word, which is how `SOCKET` and `int` share a register.
const FAILED: u32 = 0xffff_ffff;

/// `WSAENOTSOCK`: the call named a socket this machine never issued.
const WSAENOTSOCK: u32 = 10038;
/// `WSAENETDOWN`: the network subsystem is unavailable, which is the truth here.
const WSAENETDOWN: u32 = 10050;

/// The version a `WSAData` reports: 2.2, the highest WinSock 1.1 speaks.
const VERSION_1_1: u16 = 0x0202;
/// `WSAData` is 400 bytes, and `wVersion`, `wHighVersion` and the two
/// NUL-terminated strings are the only fields with a shape.
const WSADATA_BYTES: u32 = 400;

/// Every ordinal this host answers, filed under its number: the image imports
/// them by ordinal, so there is no name in its import table to match.
pub const SHIMS: &[Binding] = &[
    Binding { dll: WSOCK32, name: "#1", arg_bytes: 12, shim: accept },
    Binding { dll: WSOCK32, name: "#2", arg_bytes: 12, shim: bind },
    Binding { dll: WSOCK32, name: "#3", arg_bytes: 4, shim: closesocket },
    Binding { dll: WSOCK32, name: "#4", arg_bytes: 12, shim: connect },
    Binding { dll: WSOCK32, name: "#5", arg_bytes: 12, shim: getpeername },
    Binding { dll: WSOCK32, name: "#6", arg_bytes: 12, shim: getsockname },
    Binding { dll: WSOCK32, name: "#7", arg_bytes: 20, shim: getsockopt },
    Binding { dll: WSOCK32, name: "#8", arg_bytes: 4, shim: htonl },
    Binding { dll: WSOCK32, name: "#9", arg_bytes: 4, shim: htons },
    Binding { dll: WSOCK32, name: "#10", arg_bytes: 4, shim: inet_addr },
    Binding { dll: WSOCK32, name: "#11", arg_bytes: 4, shim: inet_ntoa },
    Binding { dll: WSOCK32, name: "#12", arg_bytes: 12, shim: ioctlsocket },
    Binding { dll: WSOCK32, name: "#13", arg_bytes: 8, shim: listen },
    Binding { dll: WSOCK32, name: "#14", arg_bytes: 4, shim: ntohl },
    Binding { dll: WSOCK32, name: "#15", arg_bytes: 4, shim: ntohs },
    Binding { dll: WSOCK32, name: "#16", arg_bytes: 16, shim: recv },
    Binding { dll: WSOCK32, name: "#17", arg_bytes: 24, shim: recvfrom },
    Binding { dll: WSOCK32, name: "#18", arg_bytes: 20, shim: select },
    Binding { dll: WSOCK32, name: "#19", arg_bytes: 16, shim: send },
    Binding { dll: WSOCK32, name: "#20", arg_bytes: 24, shim: sendto },
    Binding { dll: WSOCK32, name: "#21", arg_bytes: 20, shim: setsockopt },
    Binding { dll: WSOCK32, name: "#22", arg_bytes: 8, shim: shutdown },
    Binding { dll: WSOCK32, name: "#23", arg_bytes: 12, shim: socket },
    Binding { dll: WSOCK32, name: "#51", arg_bytes: 12, shim: gethostbyaddr },
    Binding { dll: WSOCK32, name: "#52", arg_bytes: 4, shim: gethostbyname },
    Binding { dll: WSOCK32, name: "#57", arg_bytes: 8, shim: gethostname },
    Binding { dll: WSOCK32, name: "#101", arg_bytes: 16, shim: async_select },
    Binding { dll: WSOCK32, name: "#115", arg_bytes: 8, shim: startup },
    Binding { dll: WSOCK32, name: "#116", arg_bytes: 0, shim: cleanup },
];

/// `WSAStartup(version, &data)`: the library is there, with no sockets in it.
///
/// Success is the whole point of this one. A program that is told the library is
/// missing will not open its menu at all, while a program that is told there are
/// zero sockets simply fails when it asks for one -- and a game with a single
/// player mode already handles that.
fn startup(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let data = call.arg(1)?;
    if data != 0 {
        call.zero(data, WSADATA_BYTES)?;
        call.write_u16(data, VERSION_1_1)?;
        call.write_u16(data + 2, VERSION_1_1)?;
        call.write_cstr(data + 4, "WinSock 1.1")?;
        // `szSystemStatus`, `iMaxSockets`, `iMaxUdpDg` and `lpVendorInfo` stay
        // zero: no sockets, no datagrams, no vendor.
    }
    Ok(Return(0))
}

/// `WSACleanup()`: nothing was started, so nothing is torn down.
fn cleanup(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `WSAAsyncSelect(socket, window, message, events)`: accepted and never fired.
///
/// This is how a WinSock 1.1 program waits for its connection: it asks Windows to
/// post a message when the socket is readable. There is no socket, so the message
/// never comes, but refusing the registration would fail during start-up rather
/// than when the game tries to connect -- the wrong place for it.
fn async_select(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    // A caller that registers for events has reached the online code: worth
    // saying, since nothing will ever arrive and the screen will look stuck.
    if !host.notices.iter().any(|notice| notice.starts_with("WSOCK32: WSAAsyncSelect")) {
        host.notice(
            "WSOCK32: WSAAsyncSelect registered, but this host has no network, so no event will arrive"
                .to_string(),
        );
    }
    Ok(Return(0))
}

/// `socket(family, type, protocol)`: refused, the way a machine with no stack
/// refuses it.
///
/// The notice is said once and names what was asked for, because the failure of
/// an online menu to open is otherwise indistinguishable from a missing shim.
fn socket(host: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    if !host.notices.iter().any(|notice| notice.starts_with("WSOCK32: socket")) {
        host.notice(format!(
            "WSOCK32: socket(family {}, type {}, protocol {}) was refused with WSAENETDOWN: \
             this host has no network, so the program falls back to what it can do offline",
            call.arg(0)?,
            call.arg(1)?,
            call.arg(2)?,
        ));
    }
    host.last_error = WSAENETDOWN;
    Ok(Return(FAILED))
}

/// A call that names a socket: `WSAENOTSOCK`, since no socket was ever issued.
fn no_such_socket(host: &mut Host) -> Result<Action> {
    host.last_error = WSAENOTSOCK;
    Ok(Return(FAILED))
}

fn accept(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn bind(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn connect(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn listen(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn send(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn sendto(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn recv(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn recvfrom(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn getpeername(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn getsockname(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn getsockopt(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn setsockopt(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn ioctlsocket(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn select(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

fn shutdown(host: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    no_such_socket(host)
}

/// `closesocket(socket)`: nothing to close, and no error about it.
///
/// A close is the end of a path that has already failed, and failing it again
/// only gives a program a second error to handle on the way out.
fn closesocket(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `inet_ntoa(address)`: a null pointer, because there is no address to render.
fn inet_ntoa(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// `inet_addr(text)`: the dotted quad as the word a socket address holds.
///
/// The bytes of a network address are in transmission order, so `1.2.3.4` is the
/// four bytes `01 02 03 04`, which a little-endian word spells `0x04030201`. A
/// caller stores this straight into `sockaddr_in.sin_addr`, so getting the order
/// right matters even on a host that never connects.
fn inet_addr(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let text = call.cstr(0).unwrap_or_default();
    Ok(Return(dotted_quad(&text).unwrap_or(FAILED)))
}

/// `htons(value)` and `ntohs(value)`: the same swap, since they differ only in
/// which direction the caller means.
fn htons(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from((call.arg(0)? as u16).swap_bytes())))
}

fn ntohs(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(u32::from((call.arg(0)? as u16).swap_bytes())))
}

/// `htonl(value)` and `ntohl(value)`: the whole word, byte-reversed.
fn htonl(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?.swap_bytes()))
}

fn ntohl(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    Ok(Return(call.arg(0)?.swap_bytes()))
}

/// `gethostname(name, length)`: the empty name, which is a name.
///
/// A machine with no network still has a host name, and it is usually the empty
/// string; writing it keeps a caller's buffer terminated instead of leaving
/// whatever was on its stack to be printed.
fn gethostname(_: &mut Host, call: &mut Call<'_>) -> Result<Action> {
    let name = call.arg(0)?;
    let length = call.arg(1)?;
    if name != 0 && length > 0 {
        call.write(name, &[0])?;
    }
    Ok(Return(0))
}

/// `gethostbyname(name)` and `gethostbyaddr(...)`: no such host, as a machine
/// with no DNS answers.
fn gethostbyname(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

fn gethostbyaddr(_: &mut Host, _: &mut Call<'_>) -> Result<Action> {
    Ok(Return(0))
}

/// Four decimal octets into the word `sin_addr` wants, or `None` for text that is
/// not a dotted quad.
fn dotted_quad(text: &str) -> Option<u32> {
    let mut parts = text.split('.');
    let mut octets = [0_u8; 4];
    for octet in octets.iter_mut() {
        let part = parts.next()?;
        let value: u32 = part.parse().ok()?;
        if value > 255 {
            return None;
        }
        *octet = value as u8;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(
        u32::from(octets[0])
            | u32::from(octets[1]) << 8
            | u32::from(octets[2]) << 16
            | u32::from(octets[3]) << 24,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dotted_quad_is_stored_in_transmission_order() {
        // `1.2.3.4` is four bytes `01 02 03 04`, so the little-endian word is
        // `0x04030201`: a caller stores this and the wire sees 1.2.3.4.
        assert_eq!(dotted_quad("1.2.3.4"), Some(0x0403_0201));
        assert_eq!(dotted_quad("127.0.0.1"), Some(0x0100_007f));
        // Anything that is not four octets in range is `INADDR_NONE`.
        assert_eq!(dotted_quad("127.0.0"), None);
        assert_eq!(dotted_quad("1.2.3.4.5"), None);
        assert_eq!(dotted_quad("1.2.3.256"), None);
        assert_eq!(dotted_quad("localhost"), None);
    }
}
