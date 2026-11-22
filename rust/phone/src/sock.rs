//! Raw WinSock helpers (public so tools can use the sockets `open_adb` and `open_usbmux` return). The C++ engine calls `select`/`recv`/`send` on bare SOCKETs shared between
//! threads (`shutdown` from one, `recv` from another), so this port keeps the same shape: a socket is
//! a `usize` (INVALID_SOCKET is `usize::MAX`). Sockets are created by `std::net` (which also
//! initialises WinSock) and taken over with `into_raw_socket`.

use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::os::windows::io::IntoRawSocket;

use windows::Win32::Networking::WinSock as ws;

pub type Socket = usize;
pub const NO_SOCKET: Socket = usize::MAX;

pub fn from_stream(stream: TcpStream) -> Socket {
    stream.into_raw_socket() as Socket
}

/// A blocking socket to 127.0.0.1:`port` with Nagle off, or `NO_SOCKET`.
pub fn connect_loopback(port: u16) -> Socket {
    match TcpStream::connect(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)) {
        Ok(s) => {
            let _ = s.set_nodelay(true);
            from_stream(s)
        }
        Err(_) => NO_SOCKET,
    }
}

pub fn close(s: Socket) {
    if s == NO_SOCKET {
        return;
    }
    // SAFETY: `s` is a socket this crate owns; closing a stale handle only returns an error.
    unsafe {
        ws::closesocket(ws::SOCKET(s));
    }
}

/// `shutdown(s, SD_BOTH)`: wakes a thread blocked in `select`/`recv` on this socket.
pub fn shutdown(s: Socket) {
    if s == NO_SOCKET {
        return;
    }
    // SAFETY: plain WinSock call on a socket handle; errors are ignored like the C++.
    unsafe {
        ws::shutdown(ws::SOCKET(s), ws::SD_BOTH);
    }
}

/// `select` for readability. `timeout_ms < 0` waits for ever. True when a read would not block
/// (data, or the connection closing).
pub fn wait_readable(s: Socket, timeout_ms: i32) -> bool {
    let mut fds = ws::FD_SET {
        fd_count: 1,
        fd_array: [ws::SOCKET(0); 64],
    };
    fds.fd_array[0] = ws::SOCKET(s);
    let tv = ws::TIMEVAL {
        tv_sec: timeout_ms / 1000,
        tv_usec: (timeout_ms % 1000) * 1000,
    };
    // SAFETY: `fds` and `tv` outlive the call; one socket in the set.
    let r = unsafe {
        ws::select(
            0,
            Some(&mut fds),
            None,
            None,
            if timeout_ms < 0 { None } else { Some(&tv) },
        )
    };
    r == 1
}

/// Like `select` with a result count: `>0` ready, `0` timeout, `<0` error (PhoneLink::ReadExact).
pub fn select_readable(s: Socket, timeout_ms: u64) -> i32 {
    let mut fds = ws::FD_SET {
        fd_count: 1,
        fd_array: [ws::SOCKET(0); 64],
    };
    fds.fd_array[0] = ws::SOCKET(s);
    let tv = ws::TIMEVAL {
        tv_sec: (timeout_ms / 1000) as i32,
        tv_usec: ((timeout_ms % 1000) * 1000) as i32,
    };
    // SAFETY: `fds` and `tv` outlive the call; one socket in the set.
    unsafe { ws::select(0, Some(&mut fds), None, None, Some(&tv)) }
}

/// `recv`: bytes read, 0 on a closed connection, negative on an error.
pub fn recv(s: Socket, buf: &mut [u8]) -> i32 {
    // SAFETY: `buf` is a valid writable slice for the length passed.
    unsafe { ws::recv(ws::SOCKET(s), buf, ws::SEND_RECV_FLAGS(0)) }
}

pub fn send(s: Socket, buf: &[u8]) -> i32 {
    // SAFETY: `buf` is a valid readable slice.
    unsafe { ws::send(ws::SOCKET(s), buf, ws::SEND_RECV_FLAGS(0)) }
}

/// Reads exactly `dst.len()` bytes; each wait up to `timeout_ms` (`< 0`: for ever).
pub fn recv_all(s: Socket, dst: &mut [u8], timeout_ms: i32) -> bool {
    let mut got = 0;
    while got < dst.len() {
        if !wait_readable(s, timeout_ms) {
            return false;
        }
        let n = recv(s, &mut dst[got..]);
        if n <= 0 {
            return false;
        }
        got += n as usize;
    }
    true
}

pub fn send_all(s: Socket, data: &[u8]) -> bool {
    let mut sent = 0;
    while sent < data.len() {
        let n = send(s, &data[sent..]);
        if n <= 0 {
            return false;
        }
        sent += n as usize;
    }
    true
}

pub fn set_opt_u32(s: Socket, level: i32, name: i32, value: u32) {
    // SAFETY: the option buffer is a live 4-byte slice; a failure is ignored like the C++.
    unsafe {
        ws::setsockopt(ws::SOCKET(s), level, name, Some(&value.to_ne_bytes()));
    }
}

pub fn set_nodelay(s: Socket) {
    set_opt_u32(s, ws::IPPROTO_TCP.0, ws::TCP_NODELAY, 1);
}

pub fn set_rcvbuf(s: Socket, bytes: u32) {
    set_opt_u32(s, ws::SOL_SOCKET, ws::SO_RCVBUF, bytes);
}

/// SO_SNDTIMEO in milliseconds (a DWORD on Windows).
pub fn set_send_timeout(s: Socket, ms: u32) {
    set_opt_u32(s, ws::SOL_SOCKET, ws::SO_SNDTIMEO, ms);
}

/// SO_RCVTIMEO in milliseconds.
pub fn set_recv_timeout(s: Socket, ms: u32) {
    set_opt_u32(s, ws::SOL_SOCKET, ws::SO_RCVTIMEO, ms);
}

/// `WSAIoctl(SIO_KEEPALIVE_VALS)`: keepalive after `time_ms` of silence, then every `interval_ms`.
pub fn set_keepalive(s: Socket, time_ms: u32, interval_ms: u32) {
    let ka = ws::tcp_keepalive {
        onoff: 1,
        keepalivetime: time_ms,
        keepaliveinterval: interval_ms,
    };
    let mut bytes = 0u32;
    // SAFETY: `ka` and `bytes` outlive the call; no overlapped I/O, so no completion routine.
    unsafe {
        ws::WSAIoctl(
            ws::SOCKET(s),
            ws::SIO_KEEPALIVE_VALS,
            Some(std::ptr::from_ref(&ka).cast()),
            std::mem::size_of::<ws::tcp_keepalive>() as u32,
            None,
            0,
            &mut bytes,
            None,
            None,
        );
    }
}

/// `ioctlsocket(FIONREAD)`: bytes received and not yet read.
pub fn pending_bytes(s: Socket) -> usize {
    let mut n = 0u32;
    // SAFETY: `n` is a live u32 for the call.
    let r = unsafe { ws::ioctlsocket(ws::SOCKET(s), ws::FIONREAD, &mut n) };
    if r == 0 {
        n as usize
    } else {
        0
    }
}
