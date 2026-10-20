// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Real non-blocking TCP connect with a pollable OS descriptor.
//!
//! A non-blocking `SocketChannel.connect()` must return immediately with a
//! socket whose connect is *in progress* and whose fd the JDK's selector can
//! poll for `OP_CONNECT` (write-readiness). Earlier waves emulated this with a
//! synchronous fast-path dial plus a background connect-pool thread; the
//! resulting `Connecting` registry entry had **no OS fd**, so the selector
//! never reported `OP_CONNECT` for it. An Apache-NIO reactor (ES RestClient)
//! that registered `OP_CONNECT` interest on such a channel saw the readiness
//! never arrive: by the time the deferred channel was actually wired into the
//! reactor, the session request's connect deadline had already fired and
//! terminated it, so `AbstractIOReactor.processNewChannels` threw
//! `CancelledKeyException` and abandoned the request **without a callback**
//! (ES `RestClientMultipleHostsIntegTests.testAsyncRequests`: a latch that
//! never reaches 0).
//!
//! This module starts a genuine non-blocking connect via raw socket FFI
//! (Windows: `Ws2_32`; Unix: `libc`) and returns the (already non-blocking)
//! socket wrapped as a std [`TcpStream`]. The connecting socket is stored in
//! the `tcp_registry` and handed to the selector like any stream, so the
//! existing `kernel_select` write-readiness path reports `OP_CONNECT`
//! naturally — **no manual `OP_CONNECT` injection**, which previously
//! double-processed the connecting reactor's session request
//! (`IllegalStateException: Session request has already been set`).
//!
//! `finishConnect()` calls [`poll`] which checks write/error readiness and
//! reads `SO_ERROR` to distinguish "still connecting" from "connected" from
//! "refused".

use std::net::{SocketAddr, TcpStream};

/// Outcome of starting a non-blocking connect.
pub enum StartConnect {
    /// `connect()` completed synchronously (can happen on warm loopback,
    /// especially on Linux). The stream is ready for I/O.
    Connected(TcpStream),
    /// `connect()` is in progress (`WSAEWOULDBLOCK` / `EINPROGRESS`). Poll the
    /// returned stream for write-readiness, then call [`poll`].
    InProgress(TcpStream),
    /// The local non-blocking socket is retained for selector registration,
    /// but a bounded loopback probe has already observed a terminal failure.
    /// The caller must surface it from `finishConnect()`, not throw from
    /// `connect()`, to preserve the asynchronous SocketChannel contract.
    DeferredFailure(TcpStream, std::io::Error),
}

/// Result of polling a connecting socket for completion.
pub enum ConnectPoll {
    /// Connect not yet resolved — neither writable nor errored.
    Pending,
    /// Connect succeeded; the stream is ready for I/O.
    Connected,
    /// Connect failed (peer refused / unreachable / timed out).
    Failed(std::io::Error),
}

#[cfg(unix)]
pub use imp_unix::{bind, poll, start, start_bound};
#[cfg(windows)]
pub use imp_windows::{bind, poll, start, start_bound};

// ---------------------------------------------------------------------------
// Windows — raw Ws2_32 FFI (mirrors net.rs / nio_selector.rs patterns)
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp_windows {
    use super::{ConnectPoll, StartConnect};
    use std::net::SocketAddr;
    use std::net::TcpStream;
    use std::os::windows::io::{AsRawSocket, FromRawSocket};

    type Socket = usize; // SOCKET = UINT_PTR
    const INVALID_SOCKET: Socket = usize::MAX;

    const AF_INET: i32 = 2;
    const AF_INET6_I: i32 = 23; // Windows AF_INET6
    const SOCK_STREAM: i32 = 1;
    const IPPROTO_TCP: i32 = 6;
    // FIONBIO = _IOW('f', 126, u_long) = 0x8004667E. `ioctlsocket` cmd is
    // `long` (i32 on Win64); use the same wrapping as the value's bit pattern.
    const FIONBIO: i32 = 0x8004_667Eu32 as i32;
    const WSAEWOULDBLOCK: i32 = 10035;
    const SOL_SOCKET: i32 = 0xffff;
    const SO_ERROR: i32 = 0x1007;

    const WSAPOLLWRNORM: i16 = 0x0010;
    const WSAPOLLERR: i16 = 0x0001;
    const WSAPOLLHUP: i16 = 0x0002;

    // Layout MUST match the `Wsapollfd` in `nio_selector.rs` (and the OS
    // WSAPOLLFD): a `(SOCKET, SHORT, SHORT)` triple.
    #[repr(C)]
    struct Wsapollfd {
        fd: usize,
        events: i16,
        revents: i16,
    }

    // NOTE: `ioctlsocket` and `WSAPoll` are declared with signatures
    // IDENTICAL to the existing decls in `net.rs` / `nio_selector.rs` so the
    // `clashing_extern_declarations` deny-lint does not fire. The remaining
    // symbols are not declared elsewhere in this crate.
    #[link(name = "Ws2_32")]
    extern "system" {
        fn socket(af: i32, ty: i32, protocol: i32) -> Socket;
        #[link_name = "bind"]
        fn ws_bind(s: Socket, name: *const u8, namelen: i32) -> i32;
        fn connect(s: Socket, name: *const u8, namelen: i32) -> i32;
        fn ioctlsocket(s: usize, cmd: i32, argp: *mut u32) -> i32;
        fn getsockopt(
            s: Socket,
            level: i32,
            optname: i32,
            optval: *mut u8,
            optlen: *mut i32,
        ) -> i32;
        fn closesocket(s: Socket) -> i32;
        fn WSAGetLastError() -> i32;
        fn WSAPoll(fd_array: *mut Wsapollfd, fds: u32, timeout: i32) -> i32;
    }

    /// Build a `sockaddr_in` (16 bytes) / `sockaddr_in6` (28 bytes) for the OS
    /// `connect`. `sin_family` is host byte order; port is network order.
    fn build_sockaddr(addr: &SocketAddr) -> Vec<u8> {
        match addr {
            SocketAddr::V4(v4) => {
                let mut b = vec![0u8; 16];
                b[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
                b[2..4].copy_from_slice(&v4.port().to_be_bytes());
                b[4..8].copy_from_slice(&v4.ip().octets());
                b
            }
            SocketAddr::V6(v6) => {
                let mut b = vec![0u8; 28];
                b[0..2].copy_from_slice(&(AF_INET6_I as u16).to_ne_bytes());
                b[2..4].copy_from_slice(&v6.port().to_be_bytes());
                // 4..8 flowinfo = 0
                b[8..24].copy_from_slice(&v6.ip().octets());
                b[24..28].copy_from_slice(&v6.scope_id().to_ne_bytes());
                b
            }
        }
    }

    pub fn start(addr: &SocketAddr) -> std::io::Result<StartConnect> {
        let af = match addr {
            SocketAddr::V4(_) => AF_INET,
            SocketAddr::V6(_) => AF_INET6_I,
        };
        // SAFETY: standard Winsock create / set-nonblocking / connect sequence.
        // The SOCKET is taken over by `TcpStream::from_raw_socket` on the
        // success/in-progress paths (its Drop calls closesocket); on the error
        // paths we close it ourselves before returning.
        unsafe {
            let s = socket(af, SOCK_STREAM, IPPROTO_TCP);
            if s == INVALID_SOCKET {
                return Err(std::io::Error::from_raw_os_error(WSAGetLastError()));
            }
            let mut nb: u32 = 1;
            if ioctlsocket(s, FIONBIO, &mut nb) != 0 {
                let e = std::io::Error::from_raw_os_error(WSAGetLastError());
                closesocket(s);
                return Err(e);
            }
            // Windows can leave a raw non-blocking loopback connect pending
            // forever after a local listener closes. Probe before starting
            // that raw connect: the ordinary blocking loopback connect gets
            // the definitive result immediately. Keep the untouched
            // non-blocking descriptor when the probe refuses so Java still
            // receives the failure from selector-driven finishConnect().
            if addr.ip().is_loopback() {
                match TcpStream::connect(addr) {
                    Ok(stream) => {
                        closesocket(s);
                        stream.set_nonblocking(true)?;
                        return Ok(StartConnect::Connected(stream));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
                        return Ok(StartConnect::DeferredFailure(
                            TcpStream::from_raw_socket(s as _),
                            error,
                        ));
                    }
                    Err(_) => {}
                }
            }
            let sa = build_sockaddr(addr);
            let rc = connect(s, sa.as_ptr(), sa.len() as i32);
            if rc == 0 {
                return Ok(StartConnect::Connected(TcpStream::from_raw_socket(s as _)));
            }
            let werr = WSAGetLastError();
            if werr == WSAEWOULDBLOCK {
                return Ok(StartConnect::InProgress(TcpStream::from_raw_socket(s as _)));
            }
            closesocket(s);
            Err(std::io::Error::from_raw_os_error(werr))
        }
    }

    /// Create a TCP socket and bind it before it is connected. The returned
    /// stream owns an unconnected OS socket; `start_bound` consumes it later.
    pub fn bind(addr: &SocketAddr) -> std::io::Result<TcpStream> {
        let af = match addr {
            SocketAddr::V4(_) => AF_INET,
            SocketAddr::V6(_) => AF_INET6_I,
        };
        // SAFETY: standard Winsock socket/bind sequence. TcpStream assumes
        // ownership only after `bind` succeeds; error paths close the socket.
        unsafe {
            let s = socket(af, SOCK_STREAM, IPPROTO_TCP);
            if s == INVALID_SOCKET {
                return Err(std::io::Error::from_raw_os_error(WSAGetLastError()));
            }
            let sa = build_sockaddr(addr);
            if ws_bind(s, sa.as_ptr(), sa.len() as i32) != 0 {
                let e = std::io::Error::from_raw_os_error(WSAGetLastError());
                closesocket(s);
                return Err(e);
            }
            Ok(TcpStream::from_raw_socket(s as _))
        }
    }

    /// Start a non-blocking connect using an already-bound socket.
    pub fn start_bound(stream: TcpStream, addr: &SocketAddr) -> std::io::Result<StartConnect> {
        use std::os::windows::io::IntoRawSocket;
        let s = stream.into_raw_socket() as Socket;
        // SAFETY: ownership of `s` was transferred out of `stream`; all paths
        // either wrap it back into TcpStream or close it.
        unsafe {
            let mut nb: u32 = 1;
            if ioctlsocket(s, FIONBIO, &mut nb) != 0 {
                let e = std::io::Error::from_raw_os_error(WSAGetLastError());
                closesocket(s);
                return Err(e);
            }
            let sa = build_sockaddr(addr);
            let rc = connect(s, sa.as_ptr(), sa.len() as i32);
            if rc == 0 {
                return Ok(StartConnect::Connected(TcpStream::from_raw_socket(s as _)));
            }
            let werr = WSAGetLastError();
            if werr == WSAEWOULDBLOCK {
                return Ok(StartConnect::InProgress(TcpStream::from_raw_socket(s as _)));
            }
            closesocket(s);
            Err(std::io::Error::from_raw_os_error(werr))
        }
    }

    pub fn poll(stream: &TcpStream) -> ConnectPoll {
        let s = stream.as_raw_socket() as usize;
        // Windows can record a loopback refusal in SO_ERROR without also
        // raising a WSAPoll writable/error edge. Probe SO_ERROR first so the
        // selector can surface OP_CONNECT and finishConnect() can report the
        // failure to its asynchronous caller.
        let mut err: i32 = 0;
        let mut len: i32 = std::mem::size_of::<i32>() as i32;
        // SAFETY: `s` is borrowed from the live TcpStream and the output
        // pointers name writable i32 storage of the advertised length.
        let rc = unsafe {
            getsockopt(
                s,
                SOL_SOCKET,
                SO_ERROR,
                &mut err as *mut i32 as *mut u8,
                &mut len,
            )
        };
        if rc != 0 {
            // SAFETY: WSAGetLastError has no pointer arguments or preconditions.
            return ConnectPoll::Failed(std::io::Error::from_raw_os_error(unsafe {
                WSAGetLastError()
            }));
        }
        if err != 0 {
            return ConnectPoll::Failed(std::io::Error::from_raw_os_error(err));
        }
        let mut pfd = Wsapollfd {
            fd: s,
            events: WSAPOLLWRNORM,
            revents: 0,
        };
        // SAFETY: single valid pollfd, zero timeout (non-blocking probe).
        let n = unsafe { WSAPoll(&mut pfd, 1, 0) };
        if n == 0 {
            return ConnectPoll::Pending;
        }
        if n < 0 {
            return ConnectPoll::Failed(std::io::Error::last_os_error());
        }
        // Some event fired. Read SO_ERROR to get the definitive verdict — it is
        // 0 on a completed connect and the connect errno on failure.
        err = 0;
        len = std::mem::size_of::<i32>() as i32;
        // SAFETY: `err`/`len` are valid out-pointers sized for an int option.
        let rc = unsafe {
            getsockopt(
                s,
                SOL_SOCKET,
                SO_ERROR,
                &mut err as *mut i32 as *mut u8,
                &mut len,
            )
        };
        if rc != 0 {
            return ConnectPoll::Failed(std::io::Error::last_os_error());
        }
        if err != 0 {
            return ConnectPoll::Failed(std::io::Error::from_raw_os_error(err));
        }
        if pfd.revents & WSAPOLLWRNORM != 0 {
            return ConnectPoll::Connected;
        }
        if pfd.revents & (WSAPOLLERR | WSAPOLLHUP) != 0 {
            return ConnectPoll::Failed(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connect failed",
            ));
        }
        ConnectPoll::Pending
    }

    #[cfg(test)]
    mod tests {
        use super::{start, StartConnect};
        #[allow(unused_imports)]
        use cratonvm_native_api::{
            NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
            NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
        };
        use std::net::TcpListener;

        #[test]
        fn closed_loopback_port_reports_connection_refused() {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback probe");
            let addr = listener.local_addr().expect("loopback address");
            drop(listener);

            match start(&addr) {
                Err(error) => assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused),
                Ok(StartConnect::Connected(_)) => panic!("closed loopback port connected"),
                Ok(StartConnect::DeferredFailure(_, error)) => {
                    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
                }
                Ok(StartConnect::InProgress(_)) => {
                    panic!("closed loopback port was not classified as a deferred refusal")
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Unix — libc (Linux epoll selector + macOS/BSD poll fallback both work over
// a real pollable fd)
// ---------------------------------------------------------------------------

#[cfg(unix)]
mod imp_unix {
    use super::{ConnectPoll, StartConnect};
    use std::net::SocketAddr;
    use std::net::TcpStream;
    use std::os::unix::io::{AsRawFd, FromRawFd};

    fn build_sockaddr(addr: &SocketAddr) -> Vec<u8> {
        match addr {
            SocketAddr::V4(v4) => {
                let mut b = vec![0u8; 16];
                b[0..2].copy_from_slice(&(libc::AF_INET as u16).to_ne_bytes());
                b[2..4].copy_from_slice(&v4.port().to_be_bytes());
                b[4..8].copy_from_slice(&v4.ip().octets());
                b
            }
            SocketAddr::V6(v6) => {
                let mut b = vec![0u8; 28];
                b[0..2].copy_from_slice(&(libc::AF_INET6 as u16).to_ne_bytes());
                b[2..4].copy_from_slice(&v6.port().to_be_bytes());
                b[8..24].copy_from_slice(&v6.ip().octets());
                b[24..28].copy_from_slice(&v6.scope_id().to_ne_bytes());
                b
            }
        }
    }

    pub fn start(addr: &SocketAddr) -> std::io::Result<StartConnect> {
        let af = match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        // SAFETY: standard BSD-socket create / set-nonblocking / connect
        // sequence. The fd is taken over by `TcpStream::from_raw_fd` on the
        // success/in-progress paths; on error paths we `close` it first.
        unsafe {
            let fd = libc::socket(af, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            let sa = build_sockaddr(addr);
            let rc = libc::connect(
                fd,
                sa.as_ptr() as *const libc::sockaddr,
                sa.len() as libc::socklen_t,
            );
            if rc == 0 {
                return Ok(StartConnect::Connected(TcpStream::from_raw_fd(fd)));
            }
            let e = std::io::Error::last_os_error();
            let in_progress = e.raw_os_error() == Some(libc::EINPROGRESS)
                || e.kind() == std::io::ErrorKind::WouldBlock;
            if in_progress {
                return Ok(StartConnect::InProgress(TcpStream::from_raw_fd(fd)));
            }
            libc::close(fd);
            Err(e)
        }
    }

    /// Create a TCP socket and bind it before it is connected.
    pub fn bind(addr: &SocketAddr) -> std::io::Result<TcpStream> {
        let af = match addr {
            SocketAddr::V4(_) => libc::AF_INET,
            SocketAddr::V6(_) => libc::AF_INET6,
        };
        // SAFETY: the descriptor becomes a TcpStream only after a successful
        // bind; every error path closes it exactly once.
        unsafe {
            let fd = libc::socket(af, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let sa = build_sockaddr(addr);
            if libc::bind(
                fd,
                sa.as_ptr() as *const libc::sockaddr,
                sa.len() as libc::socklen_t,
            ) != 0
            {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            Ok(TcpStream::from_raw_fd(fd))
        }
    }

    /// Start a non-blocking connect using an already-bound socket.
    pub fn start_bound(stream: TcpStream, addr: &SocketAddr) -> std::io::Result<StartConnect> {
        use std::os::unix::io::IntoRawFd;
        let fd = stream.into_raw_fd();
        // SAFETY: ownership of `fd` was transferred out of `stream`; all paths
        // either wrap it back into TcpStream or close it.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL, 0);
            if flags < 0 || libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
                let e = std::io::Error::last_os_error();
                libc::close(fd);
                return Err(e);
            }
            let sa = build_sockaddr(addr);
            let rc = libc::connect(
                fd,
                sa.as_ptr() as *const libc::sockaddr,
                sa.len() as libc::socklen_t,
            );
            if rc == 0 {
                return Ok(StartConnect::Connected(TcpStream::from_raw_fd(fd)));
            }
            let e = std::io::Error::last_os_error();
            let in_progress = e.raw_os_error() == Some(libc::EINPROGRESS)
                || e.kind() == std::io::ErrorKind::WouldBlock;
            if in_progress {
                return Ok(StartConnect::InProgress(TcpStream::from_raw_fd(fd)));
            }
            libc::close(fd);
            Err(e)
        }
    }

    pub fn poll(stream: &TcpStream) -> ConnectPoll {
        let fd = stream.as_raw_fd();
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: single valid pollfd, zero timeout (non-blocking probe).
        let n = unsafe { libc::poll(&mut pfd, 1, 0) };
        if n == 0 {
            return ConnectPoll::Pending;
        }
        if n < 0 {
            return ConnectPoll::Failed(std::io::Error::last_os_error());
        }
        let mut err: libc::c_int = 0;
        let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
        // SAFETY: valid out-pointers sized for an int option.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                &mut err as *mut libc::c_int as *mut libc::c_void,
                &mut len,
            )
        };
        if rc != 0 {
            return ConnectPoll::Failed(std::io::Error::last_os_error());
        }
        if err != 0 {
            return ConnectPoll::Failed(std::io::Error::from_raw_os_error(err));
        }
        if pfd.revents & libc::POLLOUT != 0 {
            return ConnectPoll::Connected;
        }
        if pfd.revents & (libc::POLLERR | libc::POLLHUP) != 0 {
            return ConnectPoll::Failed(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "connect failed",
            ));
        }
        ConnectPoll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[allow(unused_imports)]
    use cratonvm_native_api::{
        NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
        NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
    };
    use std::net::{IpAddr, Ipv4Addr, TcpListener};
    use std::time::{Duration, Instant};

    #[test]
    fn bound_socket_connects_without_losing_its_local_port() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let peer = listener.local_addr().unwrap();
        let acceptor = std::thread::spawn(move || listener.accept().unwrap());

        let requested = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0);
        let stream = bind(&requested).unwrap();
        let bound_port = stream.local_addr().unwrap().port();
        assert_ne!(bound_port, 0, "bind(0) must allocate an outbound port");

        let stream = match start_bound(stream, &peer).unwrap() {
            StartConnect::Connected(stream) => stream,
            StartConnect::DeferredFailure(_, error) => {
                panic!("bound connect unexpectedly failed: {error}")
            }
            StartConnect::InProgress(stream) => {
                let deadline = Instant::now() + Duration::from_secs(2);
                loop {
                    match poll(&stream) {
                        ConnectPoll::Connected => break stream,
                        ConnectPoll::Failed(e) => panic!("bound connect failed: {e}"),
                        ConnectPoll::Pending if Instant::now() < deadline => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        ConnectPoll::Pending => panic!("bound connect timed out"),
                    }
                }
            }
        };
        assert_eq!(stream.local_addr().unwrap().port(), bound_port);
        drop(stream);
        drop(acceptor.join().unwrap());
    }
}
