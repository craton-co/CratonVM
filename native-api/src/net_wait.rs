// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Two socket primitives every server-side native needs and that `std::net`
//! cannot provide.
//!
//! # The listen backlog
//!
//! `std::net::TcpListener::bind` creates, binds AND listens in one call, with a
//! backlog it chooses itself (measured: 128 on Windows 11). The JDK instead
//! honours the caller's value with one rule, identical in
//! `ServerSocketChannelImpl.netBind`, `ServerSocket.bind` and
//! `sun.net.httpserver.ServerImpl`:
//!
//! ```text
//! Net.listen(fd, backlog < 1 ? 50 : backlog);
//! ```
//!
//! Measured with nothing accepting, loopback, one binary per VM:
//!
//! | requested | HotSpot 25.0.3    | CratonVM before |
//! |----------:|------------------:|----------------:|
//! | 50        | 50                | 128             |
//! | 1024      | 200 (Windows cap) | 128             |
//!
//! Over-provisioning a small backlog is a divergence nobody notices. Capping a
//! large one is not: a connect burst wider than 128 during a slow accept is
//! answered `ConnectException: Connection refused` where HotSpot queues it.
//! [`bind_tcp_listener`], and [`bind_tcp_unlistened`] followed by
//! [`listen_jdk`], are the replacement.
//!
//! # Waiting for a socket without a sleep poll
//!
//! Several accept loops here cannot simply block in `accept()`: a blocked
//! accept does not observe `close()` from another thread on Windows, nor a Java
//! interrupt. So they set the listener non-blocking and slept a fixed quantum
//! between attempts. That quantum is documented as a liveness bound, but it is
//! a LATENCY floor: a connection arriving just after the check waits out the
//! whole sleep. Measured server-side accept wait with a 10 ms quantum: p90
//! 15.9 ms against HotSpot's 2.1 ms.
//!
//! [`wait_readable_raw`] replaces the sleep with a kernel wait of the same
//! bound. It returns the instant the socket is readable (for a listener: a
//! connection is pending), so the loop keeps its close/interrupt re-check
//! cadence and loses the latency.
//!
//! **The handle is a HINT, never an authority.** Callers take the raw handle
//! under their registry lock, drop the lock, then wait, so a concurrent
//! `close()` can invalidate the handle mid-wait and a busy process can even
//! reuse the value. Both outcomes are benign by construction: the wait is
//! bounded, every error and unexpected readiness is reported as "go and look",
//! and the caller then re-takes its lock, re-checks registration and makes a
//! NON-blocking accept. The worst case is one spurious extra pass, or one wait
//! of the full bound, which is exactly what the sleep cost on every pass.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::sync::OnceLock;

fn flag_default_on(name: &str) -> bool {
    cratonvm_types::flags::runtime_var(name)
        .map(|v| {
            let v = v.trim();
            !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
        })
        .unwrap_or(true)
}

/// `CRATONVM_NET_JDK_BACKLOG` (default ON). `=0` restores `std`'s own backlog
/// at every site that routes through this module.
pub fn jdk_backlog_enabled() -> bool {
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| flag_default_on("CRATONVM_NET_JDK_BACKLOG"))
}

/// `CRATONVM_NET_EVENT_WAITS` (default ON). `=0` restores the fixed sleep
/// between accept attempts, and between request-queue checks in the embedded
/// HTTP server, so the latency change can be A/B'd inside one binary.
pub fn event_waits_enabled() -> bool {
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| flag_default_on("CRATONVM_NET_EVENT_WAITS"))
}

/// The JDK's listen-backlog rule.
pub fn jdk_backlog(requested: i32) -> i32 {
    if requested < 1 {
        50
    } else {
        requested
    }
}

/// The backlog `std::net::TcpListener::bind` uses, for the `=0` arm of
/// [`jdk_backlog_enabled`] on sites that cannot call `bind` itself.
pub const STD_BACKLOG: i32 = 128;

/// The backlog to pass to `listen()` for a caller's requested value, honouring
/// the kill switch.
pub fn effective_backlog(requested: i32) -> i32 {
    if jdk_backlog_enabled() {
        jdk_backlog(requested)
    } else {
        STD_BACKLOG
    }
}

/// Create and bind a TCP socket for `addr` WITHOUT listening, with the socket
/// setup `std::net::TcpListener::bind` performs: `SO_REUSEADDR` on Unix only
/// (std deliberately leaves it off on Windows, where it permits port stealing),
/// and a non-inheritable handle (socket2's default, as std's).
pub fn bind_tcp_unlistened(addr: SocketAddr) -> io::Result<socket2::Socket> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(addr),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    #[cfg(not(target_os = "windows"))]
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    Ok(socket)
}

/// [`bind_tcp_unlistened`], handed back as a `TcpListener` so callers need no
/// `socket2` dependency of their own. It must be `listen()`-ed through
/// [`listen_existing`] before it can accept.
pub fn bind_tcp_unlistened_listener(addr: SocketAddr) -> io::Result<TcpListener> {
    bind_tcp_unlistened(addr).map(Into::into)
}

/// `listen()` a bound socket with the JDK's rule applied to `requested`.
pub fn listen_jdk(socket: &socket2::Socket, requested: i32) -> io::Result<()> {
    socket.listen(effective_backlog(requested))
}

/// `listen()` a socket that is already wrapped as a `TcpListener` but was bound
/// through [`bind_tcp_unlistened`] and not yet listened.
pub fn listen_existing(listener: &TcpListener, requested: i32) -> io::Result<()> {
    socket2::SockRef::from(listener).listen(effective_backlog(requested))
}

/// Drop-in for `TcpListener::bind(addr)` that honours the caller's backlog.
pub fn bind_tcp_listener(addr: SocketAddr, requested_backlog: i32) -> io::Result<TcpListener> {
    if !jdk_backlog_enabled() {
        return TcpListener::bind(addr);
    }
    let socket = bind_tcp_unlistened(addr)?;
    listen_jdk(&socket, requested_backlog)?;
    Ok(socket.into())
}

/// Raw OS socket handle, as [`wait_readable_raw`] takes it.
#[cfg(windows)]
pub type RawSock = u64;
/// Raw OS socket handle, as [`wait_readable_raw`] takes it.
#[cfg(unix)]
pub type RawSock = i32;

/// The raw handle of anything that owns a socket.
#[cfg(windows)]
pub fn raw_sock<T: std::os::windows::io::AsRawSocket>(s: &T) -> RawSock {
    s.as_raw_socket()
}
/// The raw handle of anything that owns a socket.
#[cfg(unix)]
pub fn raw_sock<T: std::os::fd::AsRawFd>(s: &T) -> RawSock {
    s.as_raw_fd()
}

/// Wait up to `timeout_ms` for `raw` to become readable (for a listener: a
/// connection is pending).
///
/// `true` means "readable, or something the caller should look at": errors,
/// hang-ups and an invalid handle all report `true`, so a closed socket is
/// noticed on the caller's next re-check rather than after the timeout.
/// `false` is a plain timeout, or a signal interrupting the wait; the caller
/// loops either way. See the module docs for why the handle is only a hint.
#[cfg(windows)]
pub fn wait_readable_raw(raw: RawSock, timeout_ms: i32) -> bool {
    #[repr(C)]
    struct WsaPollFd {
        fd: usize,
        events: i16,
        revents: i16,
    }
    #[link(name = "Ws2_32")]
    extern "system" {
        fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
    }
    const POLLRDNORM: i16 = 0x0100;
    let mut pfd = WsaPollFd {
        fd: raw as usize,
        events: POLLRDNORM,
        revents: 0,
    };
    // SAFETY: a valid one-element WSAPOLLFD array on the stack. The handle may
    // be stale (see module docs); WSAPoll then reports POLLNVAL or fails, and
    // both are surfaced as "go and look".
    let n = unsafe { WSAPoll(&mut pfd, 1, timeout_ms) };
    n != 0
}

/// See the Windows twin.
#[cfg(unix)]
pub fn wait_readable_raw(raw: RawSock, timeout_ms: i32) -> bool {
    let mut pfd = libc::pollfd {
        fd: raw,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: a valid one-element pollfd array on the stack.
    let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
    if n < 0 {
        return std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR);
    }
    n != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    #[test]
    fn jdk_rule_matches_the_jdk_source() {
        assert_eq!(jdk_backlog(-1), 50);
        assert_eq!(jdk_backlog(0), 50);
        assert_eq!(jdk_backlog(1), 1);
        assert_eq!(jdk_backlog(1024), 1024);
    }

    fn queued_without_accept(listener: &TcpListener, attempts: usize) -> usize {
        let addr = listener.local_addr().unwrap();
        let mut held = Vec::new();
        for _ in 0..attempts {
            match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
                Ok(s) => held.push(s),
                Err(_) => break,
            }
        }
        held.len()
    }

    /// The defect this module exists for. Windows caps the queue at 200, so the
    /// assertion is "more than std's 128", not "== requested".
    #[test]
    fn a_large_requested_backlog_queues_more_than_std_would() {
        let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 1024).unwrap();
        let queued = queued_without_accept(&listener, 180);
        assert!(queued > STD_BACKLOG as usize, "only {queued} queued with backlog 1024");
    }

    #[test]
    fn a_small_requested_backlog_is_not_widened_to_std_s() {
        let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 5).unwrap();
        let queued = queued_without_accept(&listener, 60);
        assert!(queued < 60, "backlog 5 queued all {queued} connects");
    }

    #[test]
    fn deferred_listen_on_an_unlistened_listener_applies_the_backlog() {
        let socket = bind_tcp_unlistened("127.0.0.1:0".parse().unwrap()).unwrap();
        let listener: TcpListener = socket.into();
        listen_existing(&listener, 1024).unwrap();
        let queued = queued_without_accept(&listener, 180);
        assert!(queued > STD_BACKLOG as usize, "only {queued} queued after deferred listen");
    }

    #[test]
    fn wait_returns_when_a_connection_is_pending_not_at_the_timeout() {
        let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 50).unwrap();
        let addr = listener.local_addr().unwrap();
        let raw = raw_sock(&listener);
        let client = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            TcpStream::connect(addr).unwrap()
        });
        let start = Instant::now();
        let ready = wait_readable_raw(raw, 5_000);
        let waited = start.elapsed();
        let _keep = client.join().unwrap();
        assert!(ready);
        assert!(waited < Duration::from_millis(2_000), "waited {waited:?}");
    }

    #[test]
    fn wait_times_out_quietly_on_an_idle_listener() {
        let listener = bind_tcp_listener("127.0.0.1:0".parse().unwrap(), 50).unwrap();
        let start = Instant::now();
        assert!(!wait_readable_raw(raw_sock(&listener), 30));
        assert!(start.elapsed() >= Duration::from_millis(20));
    }
}
