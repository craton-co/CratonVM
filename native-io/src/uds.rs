// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! AF_UNIX (Unix-domain socket) primitives for the NIO channel layer.
//!
//! `java.nio.channels.{Server,}SocketChannel.open(StandardProtocolFamily.UNIX)`
//! (JDK 16+) is used by Tomcat's `NioEndpoint` when a connector is configured
//! with `unixDomainSocketPath`. CratonVM implements the NIO channel surface
//! natively (see `socket_channel.rs`), so the UDS variant has to be provided
//! here rather than by letting the real JDK's `sun.nio.ch.UnixDomainSockets`
//! bytecode run — its channels would not be CratonVM channels and would not be
//! poll-able by our selector.
//!
//! `std` has no portable AF_UNIX support (and none at all on Windows), so the
//! listener side is implemented directly on the platform socket API:
//!
//!   * **Windows**: raw `Ws2_32` FFI. Windows 10 1803+ ships `afunix.sys` and
//!     supports `AF_UNIX` + `SOCK_STREAM` with a filesystem-backed path — the
//!     same model the JDK uses there.
//!   * **Unix**: the same calls via `libc`.
//!
//! ## Why accepted/connected streams come back as `TcpStream`
//!
//! Once an AF_UNIX connection exists, every operation the channel layer
//! performs on it — `recv` / `send` / `shutdown` / `FIONBIO` / handle
//! duplication for the selector — is protocol-agnostic, and `std`'s `TcpStream`
//! is a thin owning wrapper over exactly that handle. Adopting the accepted
//! socket with `from_raw_socket` / `from_raw_fd` therefore lets the whole
//! existing `TcpHandle::Stream` path (read, write, close, selector
//! registration) work unchanged, instead of duplicating it for a second stream
//! type. The one operation that is NOT protocol-agnostic is address decoding —
//! `local_addr()` / `peer_addr()` fail with `InvalidInput` because the returned
//! `sockaddr` is not `AF_INET`/`AF_INET6` — so every call site that needs an
//! address for a UDS channel reads the path out of the synthetic channel state
//! instead (see `socket_channel::F_UDS_PATH`). All existing call sites already
//! treat `local_addr()`/`peer_addr()` as fallible (`.ok()` / `.unwrap_or…`).

use std::io;

/// Longest `sun_path` a `sockaddr_un` can carry, on both Windows (`afunix.h`
/// `UNIX_PATH_MAX`) and Linux.
pub const UNIX_PATH_MAX: usize = 108;

#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrUn {
    family: u16,
    path: [u8; UNIX_PATH_MAX],
}

impl SockAddrUn {
    fn new(path: &str) -> io::Result<(Self, i32)> {
        let bytes = path.as_bytes();
        // Reserve one byte for the NUL terminator.
        if bytes.len() >= UNIX_PATH_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Unix domain socket path is too long ({} bytes, max {}): {path}",
                    bytes.len(),
                    UNIX_PATH_MAX - 1
                ),
            ));
        }
        let mut addr = SockAddrUn {
            family: AF_UNIX as u16,
            path: [0u8; UNIX_PATH_MAX],
        };
        addr.path[..bytes.len()].copy_from_slice(bytes);
        // `sizeof(sa_family_t) + strlen(path) + 1` — the portable minimal
        // length. Windows also accepts the full struct size, but the exact
        // length is what Linux requires for a filesystem-namespace socket.
        let len = (std::mem::size_of::<u16>() + bytes.len() + 1) as i32;
        Ok((addr, len))
    }

    /// Decode the `sun_path` of a `sockaddr_un` filled in by
    /// `accept`/`getsockname`. An unnamed socket (the normal case for the
    /// client end of a UDS connection) yields an empty string.
    fn path_string(&self, len: i32) -> String {
        let prefix = std::mem::size_of::<u16>() as i32;
        if len <= prefix {
            return String::new();
        }
        let n = ((len - prefix) as usize).min(UNIX_PATH_MAX);
        let raw = &self.path[..n];
        let end = raw.iter().position(|b| *b == 0).unwrap_or(raw.len());
        String::from_utf8_lossy(&raw[..end]).into_owned()
    }
}

// ---------------------------------------------------------------------------
// Platform primitives
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod sys {
    use super::SockAddrUn;
    use std::io;

    pub type RawSock = usize;
    /// `INVALID_SOCKET` is `(SOCKET)(~0)`.
    pub const INVALID: RawSock = usize::MAX;
    pub const AF_UNIX: i32 = 1;
    const SOCK_STREAM: i32 = 1;
    const SOCKET_ERROR: i32 = -1;
    /// `FIONBIO` = `_IOW('f', 126, u_long)`.
    const FIONBIO: i32 = -2147195266; // 0x8004667E as i32
    /// `SD_SEND`.
    pub const SHUT_WR: i32 = 1;

    #[link(name = "Ws2_32")]
    extern "system" {
        fn WSAStartup(version: u16, data: *mut u8) -> i32;
        fn WSAGetLastError() -> i32;
        fn socket(af: i32, ty: i32, protocol: i32) -> RawSock;
        fn bind(s: RawSock, name: *const u8, namelen: i32) -> i32;
        fn listen(s: RawSock, backlog: i32) -> i32;
        fn accept(s: RawSock, addr: *mut u8, addrlen: *mut i32) -> RawSock;
        fn connect(s: RawSock, name: *const u8, namelen: i32) -> i32;
        fn closesocket(s: RawSock) -> i32;
        fn shutdown(s: RawSock, how: i32) -> i32;
        fn getsockname(s: RawSock, name: *mut u8, namelen: *mut i32) -> i32;
        fn ioctlsocket(s: RawSock, cmd: i32, argp: *mut u32) -> i32;
    }

    /// Winsock must be initialised before the first `socket()` call. `std`
    /// does this lazily inside its own net code, which may not have run yet
    /// when a UDS connector is the first socket the VM opens. `WSAStartup` is
    /// reference-counted and idempotent, and we deliberately never call
    /// `WSACleanup` (matching `std`).
    fn ensure_winsock() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // 512 >= sizeof(WSADATA) on every Windows ABI.
            let mut data = [0u8; 512];
            // SAFETY: `data` is a writable buffer at least as large as
            // WSADATA; 0x0202 requests Winsock 2.2, which every supported
            // Windows provides.
            unsafe {
                WSAStartup(0x0202, data.as_mut_ptr());
            }
        });
    }

    pub fn last_error() -> io::Error {
        // SAFETY: no preconditions; reads the calling thread's last Winsock error.
        io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
    }

    pub fn new_stream_socket() -> io::Result<RawSock> {
        ensure_winsock();
        // SAFETY: plain Winsock call with constant arguments.
        let s = unsafe { socket(AF_UNIX, SOCK_STREAM, 0) };
        if s == INVALID {
            return Err(last_error());
        }
        Ok(s)
    }

    pub fn bind_to(s: RawSock, addr: &SockAddrUn, len: i32) -> io::Result<()> {
        // SAFETY: `addr` is a live `sockaddr_un` and `len` is its valid prefix length.
        let rc = unsafe { bind(s, addr as *const SockAddrUn as *const u8, len) };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn listen_on(s: RawSock, backlog: i32) -> io::Result<()> {
        // SAFETY: `s` is a live listening-capable socket owned by the caller.
        let rc = unsafe { listen(s, backlog) };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn accept_on(s: RawSock) -> io::Result<(RawSock, String)> {
        let mut addr = SockAddrUn {
            family: 0,
            path: [0u8; super::UNIX_PATH_MAX],
        };
        let mut len = std::mem::size_of::<SockAddrUn>() as i32;
        // SAFETY: `addr`/`len` are a valid out-parameter pair sized for a sockaddr_un.
        let child = unsafe { accept(s, &mut addr as *mut SockAddrUn as *mut u8, &mut len) };
        if child == INVALID {
            return Err(last_error());
        }
        Ok((child, addr.path_string(len)))
    }

    pub fn connect_to(s: RawSock, addr: &SockAddrUn, len: i32) -> io::Result<()> {
        // SAFETY: `addr` is a live `sockaddr_un` and `len` is its valid prefix length.
        let rc = unsafe { connect(s, addr as *const SockAddrUn as *const u8, len) };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn local_path(s: RawSock) -> io::Result<String> {
        let mut addr = SockAddrUn {
            family: 0,
            path: [0u8; super::UNIX_PATH_MAX],
        };
        let mut len = std::mem::size_of::<SockAddrUn>() as i32;
        // SAFETY: `addr`/`len` are a valid out-parameter pair sized for a sockaddr_un.
        let rc = unsafe { getsockname(s, &mut addr as *mut SockAddrUn as *mut u8, &mut len) };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(addr.path_string(len))
    }

    pub fn set_nonblocking(s: RawSock, nonblocking: bool) -> io::Result<()> {
        let mut flag: u32 = u32::from(nonblocking);
        // SAFETY: `flag` is a valid u_long in/out pointer for FIONBIO.
        let rc = unsafe { ioctlsocket(s, FIONBIO, &mut flag) };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn shutdown_how(s: RawSock, how: i32) -> io::Result<()> {
        // SAFETY: `s` is a live socket owned by the caller.
        let rc = unsafe { shutdown(s, how) };
        if rc == SOCKET_ERROR {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn close(s: RawSock) {
        // SAFETY: `s` is a live socket the caller is relinquishing.
        unsafe {
            closesocket(s);
        }
    }

    /// Adopt a connected AF_UNIX socket into a `std::net::TcpStream` (see the
    /// module doc for why this is sound).
    pub fn adopt_stream(s: RawSock) -> std::net::TcpStream {
        use std::os::windows::io::FromRawSocket;
        // SAFETY: `s` is a live, exclusively-owned socket handle; ownership
        // transfers to the returned TcpStream, which closes it on drop.
        unsafe { std::net::TcpStream::from_raw_socket(s as std::os::windows::io::RawSocket) }
    }
}

#[cfg(unix)]
mod sys {
    use super::SockAddrUn;
    use std::io;

    pub type RawSock = i32;
    pub const INVALID: RawSock = -1;
    pub const AF_UNIX: i32 = libc::AF_UNIX;
    pub const SHUT_WR: i32 = libc::SHUT_WR;

    pub fn last_error() -> io::Error {
        io::Error::last_os_error()
    }

    pub fn new_stream_socket() -> io::Result<RawSock> {
        // SAFETY: plain libc call with constant arguments.
        let s = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if s == INVALID {
            return Err(last_error());
        }
        Ok(s)
    }

    pub fn bind_to(s: RawSock, addr: &SockAddrUn, len: i32) -> io::Result<()> {
        // SAFETY: `addr` is a live `sockaddr_un` and `len` its valid prefix length.
        let rc = unsafe {
            libc::bind(
                s,
                addr as *const SockAddrUn as *const libc::sockaddr,
                len as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn listen_on(s: RawSock, backlog: i32) -> io::Result<()> {
        // SAFETY: `s` is a live listening-capable socket owned by the caller.
        let rc = unsafe { libc::listen(s, backlog) };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn accept_on(s: RawSock) -> io::Result<(RawSock, String)> {
        let mut addr = SockAddrUn {
            family: 0,
            path: [0u8; super::UNIX_PATH_MAX],
        };
        let mut len = std::mem::size_of::<SockAddrUn>() as libc::socklen_t;
        // SAFETY: `addr`/`len` are a valid out-parameter pair sized for a sockaddr_un.
        let child = unsafe {
            libc::accept(
                s,
                &mut addr as *mut SockAddrUn as *mut libc::sockaddr,
                &mut len,
            )
        };
        if child == INVALID {
            return Err(last_error());
        }
        Ok((child, addr.path_string(len as i32)))
    }

    pub fn connect_to(s: RawSock, addr: &SockAddrUn, len: i32) -> io::Result<()> {
        // SAFETY: `addr` is a live `sockaddr_un` and `len` its valid prefix length.
        let rc = unsafe {
            libc::connect(
                s,
                addr as *const SockAddrUn as *const libc::sockaddr,
                len as libc::socklen_t,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn local_path(s: RawSock) -> io::Result<String> {
        let mut addr = SockAddrUn {
            family: 0,
            path: [0u8; super::UNIX_PATH_MAX],
        };
        let mut len = std::mem::size_of::<SockAddrUn>() as libc::socklen_t;
        // SAFETY: `addr`/`len` are a valid out-parameter pair sized for a sockaddr_un.
        let rc = unsafe {
            libc::getsockname(
                s,
                &mut addr as *mut SockAddrUn as *mut libc::sockaddr,
                &mut len,
            )
        };
        if rc != 0 {
            return Err(last_error());
        }
        Ok(addr.path_string(len as i32))
    }

    pub fn set_nonblocking(s: RawSock, nonblocking: bool) -> io::Result<()> {
        // SAFETY: F_GETFL/F_SETFL on a live fd owned by the caller.
        let flags = unsafe { libc::fcntl(s, libc::F_GETFL) };
        if flags < 0 {
            return Err(last_error());
        }
        let new = if nonblocking {
            flags | libc::O_NONBLOCK
        } else {
            flags & !libc::O_NONBLOCK
        };
        // SAFETY: as above.
        if unsafe { libc::fcntl(s, libc::F_SETFL, new) } < 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn shutdown_how(s: RawSock, how: i32) -> io::Result<()> {
        // SAFETY: `s` is a live socket owned by the caller.
        if unsafe { libc::shutdown(s, how) } != 0 {
            return Err(last_error());
        }
        Ok(())
    }

    pub fn close(s: RawSock) {
        // SAFETY: `s` is a live fd the caller is relinquishing.
        unsafe {
            libc::close(s);
        }
    }

    /// Adopt a connected AF_UNIX socket into a `std::net::TcpStream` (see the
    /// module doc for why this is sound).
    pub fn adopt_stream(s: RawSock) -> std::net::TcpStream {
        use std::os::fd::FromRawFd;
        // SAFETY: `s` is a live, exclusively-owned fd; ownership transfers to
        // the returned TcpStream, which closes it on drop.
        unsafe { std::net::TcpStream::from_raw_fd(s) }
    }
}

#[cfg(not(any(windows, unix)))]
mod sys {
    use super::SockAddrUn;
    use std::io;

    pub type RawSock = i32;
    pub const INVALID: RawSock = -1;
    pub const AF_UNIX: i32 = 1;
    pub const SHUT_WR: i32 = 1;

    fn unsupported<T>() -> io::Result<T> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Unix domain sockets are not supported on this platform",
        ))
    }

    pub fn last_error() -> io::Error {
        io::Error::new(io::ErrorKind::Unsupported, "no AF_UNIX on this platform")
    }
    pub fn new_stream_socket() -> io::Result<RawSock> {
        unsupported()
    }
    pub fn bind_to(_s: RawSock, _a: &SockAddrUn, _l: i32) -> io::Result<()> {
        unsupported()
    }
    pub fn listen_on(_s: RawSock, _b: i32) -> io::Result<()> {
        unsupported()
    }
    pub fn accept_on(_s: RawSock) -> io::Result<(RawSock, String)> {
        unsupported()
    }
    pub fn connect_to(_s: RawSock, _a: &SockAddrUn, _l: i32) -> io::Result<()> {
        unsupported()
    }
    pub fn local_path(_s: RawSock) -> io::Result<String> {
        unsupported()
    }
    pub fn set_nonblocking(_s: RawSock, _n: bool) -> io::Result<()> {
        unsupported()
    }
    pub fn shutdown_how(_s: RawSock, _h: i32) -> io::Result<()> {
        unsupported()
    }
    pub fn close(_s: RawSock) {}
    pub fn adopt_stream(_s: RawSock) -> std::net::TcpStream {
        unreachable!("adopt_stream is unreachable without AF_UNIX support")
    }
}

pub use sys::RawSock;
use sys::AF_UNIX;

/// True when this build/platform can create AF_UNIX stream sockets at all.
/// Mirrors `sun.nio.ch.UnixDomainSockets.isSupported()`.
pub fn is_supported() -> bool {
    cfg!(any(windows, unix))
}

// ---------------------------------------------------------------------------
// Listener
// ---------------------------------------------------------------------------

/// A bound, listening AF_UNIX socket. Owns the OS handle and (unless
/// `keep_path` was requested) removes the socket file when dropped — matching
/// what the JDK does not do automatically but every UDS server is expected to,
/// and what Tomcat's `NioEndpoint.unbind()` relies on so a restart can re-bind
/// the same path.
pub struct UdsListener {
    sock: RawSock,
    path: String,
}

impl UdsListener {
    /// Create, bind and listen on `path`. Fails with `AddrInUse` when the
    /// path already exists (AF_UNIX `bind` does not replace an existing file).
    pub fn bind(path: &str, backlog: i32) -> io::Result<Self> {
        let (addr, len) = SockAddrUn::new(path)?;
        let sock = sys::new_stream_socket()?;
        let guard = SockGuard(sock);
        sys::bind_to(sock, &addr, len)?;
        sys::listen_on(sock, if backlog < 1 { 50 } else { backlog })?;
        std::mem::forget(guard);
        Ok(UdsListener {
            sock,
            path: path.to_string(),
        })
    }

    /// The path this listener is bound to.
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Raw OS handle, for the selector's kernel wait.
    pub fn raw(&self) -> RawSock {
        self.sock
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        sys::set_nonblocking(self.sock, nonblocking)
    }

    /// Accept one connection. Returns the accepted socket adopted as a
    /// `TcpStream` (see the module doc) plus the peer's path, which is empty
    /// for the usual unnamed client socket.
    pub fn accept(&self) -> io::Result<(std::net::TcpStream, String)> {
        let (child, peer) = sys::accept_on(self.sock)?;
        Ok((sys::adopt_stream(child), peer))
    }
}

impl Drop for UdsListener {
    fn drop(&mut self) {
        sys::close(self.sock);
        // Remove the filesystem entry so a subsequent bind() to the same path
        // succeeds. AF_UNIX bind() fails with EADDRINUSE against a leftover
        // socket file, so leaving it behind would make a connector restart
        // (Tomcat stop/start on the same `unixDomainSocketPath`) fail.
        if !self.path.is_empty() {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Closes a raw socket unless `mem::forget`ten — used so a failure partway
/// through `bind()` does not leak the handle.
struct SockGuard(RawSock);

impl Drop for SockGuard {
    fn drop(&mut self) {
        if self.0 != sys::INVALID {
            sys::close(self.0);
        }
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

/// Connect to the AF_UNIX socket at `path`. The returned stream is adopted as
/// a `TcpStream` (see the module doc) and is in blocking mode; callers apply
/// their channel's blocking flag afterwards.
///
/// A UDS connect completes (or is refused) synchronously against the local
/// kernel — there is no handshake to wait for — so this never reports
/// "in progress" the way a TCP connect can.
pub fn connect(path: &str) -> io::Result<std::net::TcpStream> {
    let (addr, len) = SockAddrUn::new(path)?;
    let sock = sys::new_stream_socket()?;
    let guard = SockGuard(sock);
    sys::connect_to(sock, &addr, len)?;
    std::mem::forget(guard);
    Ok(sys::adopt_stream(sock))
}

/// `getsockname` on a stream previously adopted from an AF_UNIX socket.
/// Returns the empty string for an unnamed socket.
pub fn stream_local_path(stream: &std::net::TcpStream) -> io::Result<String> {
    sys::local_path(raw_of(stream))
}

/// Half-close the write side of an adopted AF_UNIX stream. `TcpStream::shutdown`
/// works here too, but going through the same `sys` layer keeps the AF_UNIX
/// paths uniform and avoids `std`'s address-family assumptions.
pub fn shutdown_write(stream: &std::net::TcpStream) -> io::Result<()> {
    sys::shutdown_how(raw_of(stream), sys::SHUT_WR)
}

#[cfg(windows)]
fn raw_of(stream: &std::net::TcpStream) -> RawSock {
    use std::os::windows::io::AsRawSocket;
    stream.as_raw_socket() as RawSock
}

#[cfg(unix)]
fn raw_of(stream: &std::net::TcpStream) -> RawSock {
    use std::os::fd::AsRawFd;
    stream.as_raw_fd()
}

#[cfg(not(any(windows, unix)))]
fn raw_of(_stream: &std::net::TcpStream) -> RawSock {
    sys::INVALID
}

/// The `AF_UNIX` constant for the current platform (exported for tests).
pub const fn af_unix() -> i32 {
    AF_UNIX
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_too_long_is_rejected() {
        let long = "x".repeat(UNIX_PATH_MAX);
        let err = match SockAddrUn::new(&long) {
            Ok(_) => panic!("an over-long sun_path must be rejected"),
            Err(e) => e,
        };
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn addr_round_trips_the_path() {
        let (addr, len) = SockAddrUn::new("/tmp/x.sock").expect("addr");
        assert_eq!(addr.family as i32, af_unix());
        assert_eq!(len as usize, 2 + "/tmp/x.sock".len() + 1);
        assert_eq!(addr.path_string(len), "/tmp/x.sock");
    }

    #[test]
    fn unnamed_addr_decodes_to_empty() {
        let addr = SockAddrUn {
            family: af_unix() as u16,
            path: [0u8; UNIX_PATH_MAX],
        };
        assert_eq!(addr.path_string(2), "");
    }

    #[test]
    fn round_trip_over_a_real_socket() {
        if !is_supported() {
            return;
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!("cratonvm-uds-test-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_str = path.to_string_lossy().into_owned();
        // A temp dir deep enough to overflow sun_path would make this test
        // meaningless rather than failing; skip instead.
        if path_str.len() >= UNIX_PATH_MAX {
            return;
        }
        let listener = UdsListener::bind(&path_str, 16).expect("bind");
        let server = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut stream, _peer) = listener.accept().expect("accept");
            let mut buf = [0u8; 4];
            stream.read_exact(&mut buf).expect("read");
            stream.write_all(b"pong").expect("write");
            buf
        });
        {
            use std::io::{Read, Write};
            let mut client = connect(&path_str).expect("connect");
            client.write_all(b"ping").expect("client write");
            let mut buf = [0u8; 4];
            client.read_exact(&mut buf).expect("client read");
            assert_eq!(&buf, b"pong");
        }
        assert_eq!(&server.join().expect("join"), b"ping");
        let _ = std::fs::remove_file(&path);
    }
}
