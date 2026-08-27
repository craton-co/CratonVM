// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! File descriptor table for I/O operations.

use parking_lot::{Mutex, RwLock};
use rustc_hash::FxHashMap;
// AUDIT 2026-05-16: std::collections::HashMap is unused — fd_table uses
// rustc_hash::FxHashMap (T10.9.B).
use std::collections::VecDeque;
use std::fs;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

// ---------------------------------------------------------------------------
// FileDescriptorTable
// ---------------------------------------------------------------------------

pub type FdId = u32;

const PIPE_BUFFER_INITIAL_CAPACITY: usize = 8192;
/// Hard cap for in-memory pipes. Writers get partial progress or WouldBlock
/// instead of growing the VecDeque without bound.
const PIPE_BUFFER_CAPACITY: usize = 64 * 1024;

fn stdio_write_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Disable Windows' `SIO_UDP_CONNRESET` behavior on a freshly created UDP
/// socket.
///
/// By default, Windows Sockets propagates an ICMP "port unreachable" reply
/// to an *earlier* outbound datagram as `WSAECONNRESET` (os error 10054) on
/// the *next* `recv`/`WSARecv` call on that socket — even though UDP is
/// connectionless and no such reset actually occurred. JDK's own native UDP
/// implementation on Windows explicitly disables this quirk at socket
/// creation (`NET_CreateSocket` issues this same `WSAIoctl`), which is why
/// real HotSpot's JNDI DNS client tolerates a non-responsive/rejecting DNS
/// peer while CratonVM's did not: the mongo-java-driver's `JndiDnsClient`
/// (`DefaultDnsResolver.resolveAdditionalQueryParametersFromTxtRecords`)
/// polls a connected `DatagramChannel` for a TXT record and — without this
/// fix — any ICMP port-unreachable on the query surfaces as a
/// `javax.naming.CommunicationException` wrapping this reset instead of a
/// clean timeout/retry, breaking `MongoAutoConfigurationTests.configuresProtocol`
/// and `PropertiesMongoConnectionDetailsTests.protocolCanBeConfigured`.
/// No-op on non-Windows (the other platforms don't have this behavior).
#[cfg(target_os = "windows")]
fn disable_udp_connreset(socket: &std::net::UdpSocket) {
    use std::os::windows::io::AsRawSocket;

    // winsock2.h: `#define SIO_UDP_CONNRESET _WSAIOW(IOC_VENDOR, 12)`.
    const SIO_UDP_CONNRESET: u32 = 0x9800_000C;

    #[link(name = "ws2_32")]
    unsafe extern "system" {
        fn WSAIoctl(
            s: usize,
            dw_io_control_code: u32,
            lpv_in_buffer: *mut core::ffi::c_void,
            cb_in_buffer: u32,
            lpv_out_buffer: *mut core::ffi::c_void,
            cb_out_buffer: u32,
            lpcb_bytes_returned: *mut u32,
            lp_overlapped: *mut core::ffi::c_void,
            lp_completion_routine: *mut core::ffi::c_void,
        ) -> i32;
    }

    let mut new_behavior: i32 = 0; // FALSE — do not report ICMP resets on this UDP socket.
    let mut bytes_returned: u32 = 0;
    // Best-effort: an ioctl failure here (e.g. an unsupported Windows
    // version) is not fatal — it just leaves the OS default quirk in place,
    // same as before this fix existed.
    unsafe {
        WSAIoctl(
            socket.as_raw_socket() as usize,
            SIO_UDP_CONNRESET,
            &mut new_behavior as *mut i32 as *mut core::ffi::c_void,
            size_of::<i32>() as u32,
            std::ptr::null_mut(),
            0,
            &mut bytes_returned as *mut u32,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

#[cfg(not(target_os = "windows"))]
fn disable_udp_connreset(_socket: &std::net::UdpSocket) {}


/// Open a **dual-stack** UDP socket: AF_INET6 with `IPV6_V6ONLY` off, so one
/// socket reaches both address families.
///
/// This is what `DatagramChannel.open()` gives you on HotSpot — `Net.socket`
/// picks `INET6` whenever IPv6 is available and clears `IPV6_V6ONLY` — and the
/// difference is observable from Java the moment a destination of the other
/// family shows up. CratonVM's channel was AF_INET, so
/// `DatagramChannel.send(buf, new InetSocketAddress("::1", p))` failed with
/// Windows `WSAEAFNOSUPPORT` (os error 10047) / Linux `EAFNOSUPPORT`, where
/// HotSpot sends the datagram. netty's
/// `DnsNameResolverTest.testTimeoutNotCached` points its resolver at
/// `NetUtil.LOCALHOST` — `::1` on a dual-stack host — and asserts a
/// `DnsNameResolverTimeoutException`; an immediate send failure produced a
/// plain `DnsNameResolverException` instead, so the assertion read as a
/// "wrong exception type" defect several layers above the socket.
///
/// Falls back to AF_INET when IPv6 is unavailable, which is also what the JDK
/// does. Callers that bind an EXPLICIT address keep whatever family that
/// address names — the dual-stack choice only applies to the wildcard, exactly
/// as in `Net.socket`.
fn open_udp_dual_stack_socket(port: u16) -> Result<std::net::UdpSocket, io::Error> {
    // The fallback covers exactly one condition: this host has no usable IPv6
    // stack, so the SOCKET cannot be created (or cannot be made dual-stack).
    // It must NOT cover a failing bind.
    //
    // It did, once, and the bug it caused is worth keeping written down. With
    // `Err(_) => bind v4` wrapped around the whole thing, a genuine
    // `EADDRINUSE` on `[::]:P` was swallowed and the socket silently bound
    // `0.0.0.0:P` instead — which Windows permits alongside a dual-stack v6
    // holder. So `DatagramChannel.bind(addressAlreadyInUse)` SUCCEEDED and
    // reported a different family than it was asked for, where HotSpot throws
    // `BindException`. netty's `DnsNameResolverTest.testAddressAlreadyInUse`
    // is the test that says so. An error-swallowing fallback around an
    // operation that has its own legitimate failures is never right; scope it
    // to the capability probe.
    let socket = match socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )
    .and_then(|s| s.set_only_v6(false).map(|()| s))
    {
        Ok(s) => s,
        // No IPv6 stack. The v4 wildcard is the JDK's own fallback and every
        // caller's pre-dual-stack behaviour.
        Err(_) => {
            return std::net::UdpSocket::bind(std::net::SocketAddr::from((
                std::net::Ipv4Addr::UNSPECIFIED,
                port,
            )))
        }
    };
    let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
    socket.bind(&addr.into())?;
    Ok(socket.into())
}

/// Open a **dual-stack** TCP listener on the wildcard address: AF_INET6 with
/// `IPV6_V6ONLY` off, so one listener accepts connections of both families.
///
/// This is the stream twin of [`open_udp_dual_stack_socket`], and it exists for
/// the same reason. `sun.nio.ch.Net.serverSocket` opens AF_INET6 and clears
/// `IPV6_V6ONLY` whenever IPv6 is available and the channel was not opened with
/// an explicit `StandardProtocolFamily.INET`, so on HotSpot a wildcard
/// `ServerSocketChannel.bind(new InetSocketAddress(0))` accepts an IPv6 client.
/// CratonVM bound `0.0.0.0` — a real AF_INET listener — and every `::1` client
/// got a RST.
///
/// That is not a corner case on Windows. netty's `NetUtil.LOCALHOST` prefers
/// the IPv6 loopback on a dual-stack host, so `SSLEngineTest.mySetupMutualAuth`
/// binds its server on the wildcard and then connects the client to `::1`:
/// `assertTrue(ccf.awaitUninterruptibly().isSuccess())` (SSLEngineTest.java:1302)
/// failed with `finishConnect: Connection refused` for EVERY parameterisation of
/// `testMutualAuthDiffCerts` in four `SSLEngineTest` subclasses, and read as a
/// TLS defect because `assertTrue` throws the future's cause away. See
/// `fixed-suite-bugs/netty/ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`.
///
/// `backlog <= 0` asks for the same 128 `std::net::TcpListener::bind` uses, and
/// `SO_REUSEADDR` is set on non-Windows only — again matching `TcpListener::bind`,
/// so the caller's own `SO_REUSEADDR` handling is unaffected by this path.
///
/// Falls back to the v4 wildcard when this host has no usable IPv6 stack, which
/// is also what the JDK does. The fallback covers SOCKET CREATION only: a failing
/// `bind` (`EADDRINUSE`) propagates, because swallowing it would let a bind that
/// HotSpot rejects succeed on a different family — the exact bug
/// [`open_udp_dual_stack_socket`] records in its own comment.
pub fn open_tcp_dual_stack_listener(
    port: u16,
    backlog: i32,
) -> Result<std::net::TcpListener, io::Error> {
    let socket = match socket2::Socket::new(
        socket2::Domain::IPV6,
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )
    .and_then(|s| s.set_only_v6(false).map(|()| s))
    {
        Ok(s) => s,
        // No IPv6 stack. The v4 wildcard is the JDK's own fallback and this
        // call site's pre-dual-stack behaviour.
        Err(_) => {
            return std::net::TcpListener::bind(std::net::SocketAddr::from((
                std::net::Ipv4Addr::UNSPECIFIED,
                port,
            )))
        }
    };
    #[cfg(not(target_os = "windows"))]
    socket.set_reuse_address(true)?;
    let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
    socket.bind(&addr.into())?;
    socket.listen(if backlog > 0 { backlog } else { 128 })?;
    Ok(socket.into())
}

/// The port of a WILDCARD `host:port` spec, in either family's spelling, or
/// `None` for a specific address.
///
/// Both spellings have to count. `dc_socket_addr` renders the v4 wildcard as
/// `0.0.0.0` and the v6 one as `[::]` (bracketed, since the caller appends
/// `:{port}`), and a caller that recognised only the first would bind
/// `bind(new InetSocketAddress("::", 0))` through the ordinary path — where
/// `UdpSocket::bind` takes the platform's `IPV6_V6ONLY` default, which is ON
/// for Windows. That is a v6-ONLY socket where HotSpot gives a dual-stack one:
/// the same defect as the v4 case, arrived at from the other side.
pub fn wildcard_bind_port(spec: &str) -> Option<u16> {
    for prefix in ["0.0.0.0:", "[::]:", "[0:0:0:0:0:0:0:0]:"] {
        if let Some(port) = spec.strip_prefix(prefix) {
            return port.parse::<u16>().ok();
        }
    }
    None
}

/// Render a datagram address the way the JDK's Java-visible API does:
/// a v4-mapped v6 address (`::ffff:a.b.c.d`) becomes plain `a.b.c.d`.
///
/// A dual-stack socket reports every IPv4 peer in the mapped form, and
/// `sun.nio.ch.Net` converts it back before it reaches an `InetAddress` —
/// HotSpot's `DatagramChannel.receive()` from a `127.0.0.1` sender answers
/// `/127.0.0.1`, never `/0:0:0:0:0:0:0:1%…` or `/::ffff:127.0.0.1`. Without
/// this, making the channel dual-stack would have changed every local
/// round-trip's reported peer address, which is a louder regression than the
/// bug being fixed.
///
/// The unspecified v6 address `::` is NOT mapped — HotSpot reports the v6
/// wildcard as `/[0:0:0:0:0:0:0:0]` for a dual-stack socket, and collapsing it
/// to `0.0.0.0` would contradict that.
///
/// # Do not "simplify" this to `to_ipv4()`
///
/// `Ipv6Addr::to_ipv4` also converts the deprecated **v4-COMPATIBLE** form
/// (`::a.b.c.d`, i.e. any address whose first twelve bytes are zero), so it
/// maps `::1` to `0.0.0.1`. That is not a hypothetical: it is bit-for-bit the
/// transformation Apache MINA applies under `isIPv4CompatibleAddress()`, and
/// it is what sent every netty DNS query to `0.0.0.1` in the defect this
/// function was written for. `to_ipv4_mapped` accepts only `::ffff:a.b.c.d`,
/// which is the only form that actually denotes an IPv4 peer.
pub fn unmap_v4_mapped(addr: std::net::SocketAddr) -> std::net::SocketAddr {
    match addr {
        std::net::SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(v4) => std::net::SocketAddr::from((v4, v6.port())),
            None => addr,
        },
        v4 => v4,
    }
}

/// The address to hand `sendto`/`connect` on `socket`, given a target the
/// caller named.
///
/// A dual-stack AF_INET6 socket cannot take a bare `AF_INET` sockaddr: the OS
/// answers `EAFNOSUPPORT`. The kernel wants the v4-mapped form, and producing
/// it is the caller's job — `sun.nio.ch.Net.translateToSocketAddress` does
/// exactly this conversion on the JDK's own send path. Returns the target
/// unchanged when the families already agree.
fn target_for_socket(
    socket: &std::net::UdpSocket,
    target: std::net::SocketAddr,
) -> std::net::SocketAddr {
    match (socket.local_addr(), target) {
        (Ok(std::net::SocketAddr::V6(_)), std::net::SocketAddr::V4(v4)) => {
            std::net::SocketAddr::from((v4.ip().to_ipv6_mapped(), v4.port()))
        }
        _ => target,
    }
}

/// Resolve `target` to a single socket address, applying [`target_for_socket`].
///
/// Resolution stays in one place so `udp_send` and `udp_connect` cannot drift
/// about which candidate they pick when a name yields several.
fn udp_target_addr(
    socket: &std::net::UdpSocket,
    target: &str,
) -> Result<std::net::SocketAddr, io::Error> {
    use std::net::ToSocketAddrs;
    let socket_is_v6 = matches!(socket.local_addr(), Ok(std::net::SocketAddr::V6(_)));
    let mut candidates = target.to_socket_addrs()?.peekable();
    // Prefer a candidate of the socket's own family; a dual-stack socket takes
    // either, and everything else can only use its own.
    let mut fallback = None;
    for cand in &mut candidates {
        if socket_is_v6 || cand.is_ipv4() {
            return Ok(target_for_socket(socket, cand));
        }
        fallback.get_or_insert(cand);
    }
    fallback
        .map(|c| target_for_socket(socket, c))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addr resolved"))
}

/// Connect to `addr`, trying IPv4 candidate addresses before IPv6.
///
/// `std::net::TcpStream::connect(host:port)` resolves the host and tries each
/// returned address in resolver order until one succeeds. On Windows, an
/// unqualified `localhost` resolves to `::1` (IPv6) *first*, then `127.0.0.1`
/// — the loopback hosts-file entries are commented out by default, so the DNS
/// client returns IPv6 ahead of IPv4. When the peer (e.g. an embedded Tomcat
/// started with `-Djava.net.preferIPv4Stack=true`) listens on IPv4 only, the
/// `[::1]:port` attempt is silently dropped and blocks for ~2 seconds before
/// the stack gives up and falls back to `127.0.0.1`. That ~2s-per-connect
/// stall is what made three serial WebSocket client connects exceed a 3s
/// session-idle timeout.
///
/// Re-ordering IPv4 candidates ahead of IPv6 mirrors `preferIPv4Stack=true`
/// and makes the common loopback case connect immediately, while still
/// falling back to IPv6 for genuinely IPv6-only hosts. A literal IP address
/// resolves to a single candidate, so the sort is a no-op for it.
fn connect_prefer_ipv4(addr: &str) -> io::Result<std::net::TcpStream> {
    use std::net::ToSocketAddrs;

    let mut candidates: Vec<std::net::SocketAddr> = addr.to_socket_addrs()?.collect();
    if candidates.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("could not resolve address: {addr}"),
        ));
    }
    // Stable sort: IPv4 (key 0) before IPv6 (key 1), preserving the resolver's
    // relative order within each family.
    candidates.sort_by_key(|sa| u8::from(sa.is_ipv6()));

    let mut last_err: Option<io::Error> = None;
    for sa in &candidates {
        match std::net::TcpStream::connect(sa) {
            Ok(stream) => return Ok(stream),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err
        .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, format!("connect failed: {addr}"))))
}

enum FileEntry {
    Stdin(Mutex<io::Stdin>),
    Stdout(Mutex<io::Stdout>),
    Stderr(Mutex<io::Stderr>),
    FileRead(Mutex<BufReader<fs::File>>),
    FileWrite(Mutex<BufWriter<fs::File>>),
    /// Read+write access for AsynchronousFileChannel / RandomAccessFile
    FileReadWrite(Mutex<fs::File>),
    /// UDP socket for DatagramChannel.
    ///
    /// Deliberately NOT behind a `Mutex`: every `std::net::UdpSocket` method
    /// this table calls takes `&self`, so the lock bought no safety — it only
    /// serialized send against receive. A datagram receiver parks inside
    /// `recv_from` for its whole `SO_TIMEOUT`, so holding a lock across that
    /// syscall stalled every concurrent `send` on the same socket by up to
    /// that timeout. Tomcat Tribes runs exactly that shape (one
    /// `McastServiceImpl` socket, a receiver thread polling on a 500 ms
    /// `soTimeout` and a sender thread announcing every 500 ms), so its
    /// membership announcements went out a whole poll interval late at random
    /// and peer discovery ran up to a second behind HotSpot's few
    /// milliseconds.
    UdpSocket(std::net::UdpSocket),
    /// TCP stream socket (SocketChannel)
    TcpStream(Mutex<std::net::TcpStream>),
    /// TCP listener socket (ServerSocketChannel).
    ///
    /// The `pending` queue holds connections that were `accept()`-ed off
    /// the OS backlog but not yet handed to a Java-level `tcp_accept`
    /// caller. It is kept for backward compatibility: `poll_ready` now
    /// uses a non-destructive `poll` probe and never pre-`accept()`s, so
    /// the queue normally stays empty and `tcp_accept` drains it (if a
    /// legacy/stashed entry exists) before falling back to a live
    /// `accept()`.
    TcpListener {
        listener: Mutex<std::net::TcpListener>,
        pending: Mutex<VecDeque<(std::net::TcpStream, std::net::SocketAddr)>>,
    },
    /// Read end of an in-memory pipe
    PipeRead(Arc<Mutex<VecDeque<u8>>>),
    /// Write end of an in-memory pipe
    PipeWrite(Arc<Mutex<VecDeque<u8>>>),
    /// TLS-wrapped TCP stream (SSLSocket)
    TlsStream(Mutex<native_tls::TlsStream<std::net::TcpStream>>),
    /// WP1.12 — subprocess stdout pipe (read side). Wraps the `ChildStdout`
    /// handle returned by `std::process::Command::spawn()` so the JDK's
    /// `FileInputStream.read*` natives can pull bytes through the same
    /// `fd_table().read_bytes()` API they use for files.
    ChildStdoutPipe(Mutex<std::process::ChildStdout>),
    /// WP1.12 — subprocess stderr pipe (read side).
    ChildStderrPipe(Mutex<std::process::ChildStderr>),
    /// WP1.12 — subprocess stdin pipe (write side). Bytes written go to
    /// the child process's standard input.
    ChildStdinPipe(Mutex<std::process::ChildStdin>),
    /// `ProcessBuilder.redirectErrorStream(true)` (`2>&1`): a single OS pipe
    /// whose write-end is handed to BOTH the child's stdout and stderr, so the
    /// merged output is read through one fd. getInputStream() reads this;
    /// getErrorStream() is then the empty stream.
    ChildMergedPipe(Mutex<std::io::PipeReader>),
}

/// Non-destructive socket readiness probe.
///
/// Polls the raw OS socket handle for read/write readiness **without**
/// mutating any shared socket state. Unlike a `set_nonblocking(true);
/// peek(); set_nonblocking(false)` dance, this:
///
///  * does not flip the socket's persistent blocking mode (so a
///    concurrent blocking `tcp_read` / `udp_recv` on the same fd can
///    never observe a transient non-blocking window and return a
///    spurious `WouldBlock`), and
///  * does not consume or peek datagram/stream payload.
///
/// It uses the OS `poll` (Unix) / `WSAPoll` (Windows) primitive with a
/// zero timeout, which is purely a query of kernel socket state.
///
/// Returns `(readable, writable)`. On any error the socket is reported
/// as not-readable / not-writable, matching the previous fallback
/// behaviour of `poll_ready`.
fn poll_socket_readiness<S>(sock: &S) -> (bool, bool)
where
    S: socket2_raw::AsRawHandle,
{
    socket2_raw::poll_readiness(sock.as_raw())
}

/// [`poll_socket_readiness`] with a real timeout instead of a zero one.
///
/// `None` means the poll primitive itself failed (or this target has none) —
/// the signal for the caller to fall back to ONE plain blocking call rather
/// than spin on a stub that answers "not ready" forever. That is the same
/// three-state contract `native-io::net::poll_stream_readable` states, restated
/// here because this crate cannot depend on `native-io`.
fn poll_socket_readiness_timeout(
    raw: socket2_raw::RawHandle,
    timeout_ms: i32,
) -> Option<(bool, bool)> {
    socket2_raw::poll_readiness_timeout(raw, timeout_ms)
}

/// The raw OS handle for a socket, taken so the close-aware wait can poll it
/// **without** holding the entry's `Mutex`. Safe because every caller holds the
/// entry's `Arc` for the whole operation, so the socket cannot be dropped and
/// the handle number cannot be recycled while the poll is in flight.
fn raw_handle_of<S>(sock: &S) -> socket2_raw::RawHandle
where
    S: socket2_raw::AsRawHandle,
{
    sock.as_raw()
}

/// How long a close-aware blocking primitive parks inside one poll before
/// re-asking the table whether the fd was closed under it.
///
/// A liveness bound, not a latency cost: the poll returns the instant the
/// socket becomes ready, so payload is never delayed by it. It only bounds how
/// long a reader stays parked after another thread calls `close()`. Same value
/// and same role as `native-io::net::NET_READ_CLOSE_POLL_MS`.
const FD_CLOSE_POLL_MS: i32 = 25;

/// The error a parked read/write/accept reports once its fd has been closed
/// from another thread.
///
/// `ErrorKind::Interrupted` is the same carrier the three landed close-aware
/// readers use (`net_read_close_aware`, `socket_channel::read_close_aware`,
/// `net_phase_e::re1_read_close_aware`), so the callers of this table can tell
/// an asynchronous close apart from every other failure without a new error
/// type. It is unambiguous here because the poll primitive above reports a real
/// EINTR as "not ready" and never as an error.
fn fd_async_closed_err() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "socket closed")
}

/// Thin, dependency-free wrapper over the OS `poll` / `WSAPoll`
/// readiness primitive. Kept in its own module so the platform `extern`
/// blocks and constants do not leak into the rest of `fd_table`.
mod socket2_raw {
    /// Abstraction over "give me the raw socket handle for polling".
    pub trait AsRawHandle {
        fn as_raw(&self) -> RawHandle;
    }

    #[cfg(unix)]
    pub type RawHandle = std::os::fd::RawFd;
    #[cfg(windows)]
    pub type RawHandle = std::os::windows::io::RawSocket;

    #[cfg(unix)]
    impl<T: std::os::fd::AsRawFd> AsRawHandle for T {
        fn as_raw(&self) -> RawHandle {
            self.as_raw_fd()
        }
    }
    #[cfg(windows)]
    impl<T: std::os::windows::io::AsRawSocket> AsRawHandle for T {
        fn as_raw(&self) -> RawHandle {
            self.as_raw_socket()
        }
    }

    // `nfds_t` (the `poll` count argument) is `unsigned long` on Linux
    // and the BSDs but `unsigned int` on macOS/iOS — declare it with the
    // matching width so the FFI ABI is correct on every Unix target.
    #[cfg(all(unix, any(target_os = "macos", target_os = "ios")))]
    type NfdsT = u32;
    #[cfg(all(unix, not(any(target_os = "macos", target_os = "ios"))))]
    type NfdsT = u64;

    #[cfg(unix)]
    pub fn poll_readiness(fd: RawHandle) -> (bool, bool) {
        poll_readiness_timeout(fd, 0).unwrap_or((false, false))
    }

    /// [`poll_readiness`] with a caller-chosen timeout.
    ///
    /// `None` is "the primitive failed / does not exist", NOT "not ready".
    /// Callers that park on this must fall back to one plain blocking call on
    /// `None`, or they would spin forever against a stub.
    ///
    /// EINTR is reported as `Some((false, false))` — not ready, not an error,
    /// and never an in-place re-poll. `poll(2)` is **never** auto-restarted by
    /// `SA_RESTART`, so a signal delivered to a thread parked here always
    /// returns EINTR, and this VM sends one on purpose (`jit::xt_root_scan`
    /// SIGUSR2s every thread for a cross-thread root scan). Re-polling in place
    /// with the same `timeout` would restart the whole wait on every GC and
    /// silently defeat any deadline the caller is enforcing. This mirrors
    /// `native-io::net::net_poll_raw`'s AUDIT 2026-08-02 arm exactly.
    #[cfg(unix)]
    pub fn poll_readiness_timeout(fd: RawHandle, timeout_ms: i32) -> Option<(bool, bool)> {
        // struct pollfd { int fd; short events; short revents; }
        #[repr(C)]
        struct PollFd {
            fd: i32,
            events: i16,
            revents: i16,
        }
        const POLLIN: i16 = 0x0001;
        const POLLOUT: i16 = 0x0004;
        // Error/hangup conditions also make the fd "ready" — the
        // subsequent read/write will surface the actual error rather
        // than blocking, which is the readiness contract callers want.
        const POLLERR: i16 = 0x0008;
        const POLLHUP: i16 = 0x0010;
        const POLLNVAL: i16 = 0x0020;

        extern "C" {
            fn poll(fds: *mut PollFd, nfds: NfdsT, timeout: i32) -> i32;
        }

        let mut pfd = PollFd {
            fd,
            events: POLLIN | POLLOUT,
            revents: 0,
        };
        // SAFETY: `pfd` is a single, properly-initialised `pollfd`;
        // `nfds == 1` matches the one-element buffer; `timeout_ms` is the
        // caller's bound in milliseconds (0 returns immediately).
        let rc = unsafe { poll(&mut pfd as *mut PollFd, 1 as NfdsT, timeout_ms) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted || error.raw_os_error() == Some(4) {
                return Some((false, false));
            }
            return None;
        }
        if pfd.revents & POLLNVAL != 0 {
            return None;
        }
        let err = pfd.revents & (POLLERR | POLLHUP) != 0;
        let readable = pfd.revents & POLLIN != 0 || err;
        let writable = pfd.revents & POLLOUT != 0 || err;
        Some((readable, writable))
    }

    #[cfg(windows)]
    pub fn poll_readiness(socket: RawHandle) -> (bool, bool) {
        poll_readiness_timeout(socket, 0).unwrap_or((false, false))
    }

    /// [`poll_readiness`] with a caller-chosen timeout. See the Unix twin for
    /// the `None` contract. There is no EINTR arm here: Winsock has no EINTR,
    /// and therefore no `SA_RESTART` hazard either.
    #[cfg(windows)]
    pub fn poll_readiness_timeout(socket: RawHandle, timeout_ms: i32) -> Option<(bool, bool)> {
        // WSAPOLLFD { SOCKET fd; SHORT events; SHORT revents; }
        #[repr(C)]
        struct WsaPollFd {
            fd: usize,
            events: i16,
            revents: i16,
        }
        const POLLRDNORM: i16 = 0x0100;
        const POLLWRNORM: i16 = 0x0010;
        const POLLERR: i16 = 0x0001;
        const POLLHUP: i16 = 0x0002;
        const POLLNVAL: i16 = 0x0004;

        #[link(name = "ws2_32")]
        extern "system" {
            fn WSAPoll(fds: *mut WsaPollFd, nfds: u32, timeout: i32) -> i32;
        }

        let mut pfd = WsaPollFd {
            fd: socket as usize,
            events: POLLRDNORM | POLLWRNORM,
            revents: 0,
        };
        // SAFETY: single properly-initialised WSAPOLLFD, `nfds == 1`
        // matches the buffer length; `timeout_ms` is the caller's bound.
        let rc = unsafe { WSAPoll(&mut pfd as *mut WsaPollFd, 1, timeout_ms) };
        if rc < 0 {
            return None;
        }
        if pfd.revents & POLLNVAL != 0 {
            return None;
        }
        let err = pfd.revents & (POLLERR | POLLHUP) != 0;
        let readable = pfd.revents & POLLRDNORM != 0 || err;
        let writable = pfd.revents & POLLWRNORM != 0 || err;
        Some((readable, writable))
    }
}

/// Thread-safe registry of open file handles, keyed by integer file descriptors.
///
/// Pre-registers fd 0/1/2 for stdin/stdout/stderr.
/// T10.9.B: FxHashMap — fd IDs are internal u32 counters.
///
/// AUDIT 2026-05-16 (B3/B4 native-api): entries are wrapped in `Arc` so a
/// caller can obtain a handle to a `FileEntry`, drop the table-level
/// `RwLock` read guard, and *then* perform the (possibly blocking) syscall
/// on the entry's inner `Mutex`. Holding the table read guard across a
/// syscall would serialize every `close()` / `open_*()` / `insert_*()`
/// against the longest-running I/O on any fd in the VM — a blocking
/// stdin or TCP read could freeze fd-table mutations indefinitely.
/// Bytes readable right now from a pipe, via `FIONREAD`.
///
/// Taking the entry's mutex can only block behind an in-flight read on the same
/// pipe, and the only caller that matters — the JDK's process reaper draining a
/// child that has already exited — is asking precisely because the write end is
/// gone, so that read returns promptly. Blocking there is also the safer
/// failure: returning 0 under contention would tell the drain "nothing left"
/// and lose whatever the reader has not yet consumed.
#[cfg(unix)]
fn pipe_available<T: std::os::fd::AsRawFd>(pipe: &T) -> usize {
    let mut pending: libc::c_int = 0;
    // SAFETY: `FIONREAD` writes exactly one `c_int` through the pointer, which
    // is a live local for the duration of the call.
    let rc = unsafe { libc::ioctl(pipe.as_raw_fd(), libc::FIONREAD, &mut pending) };
    if rc == 0 && pending > 0 {
        pending as usize
    } else {
        0
    }
}

/// No `FIONREAD` equivalent that is worth the Win32 surface here: `PeekNamedPipe`
/// would be the call. 0 keeps the previous behaviour on this platform, and the
/// caller that needs a real answer (`ProcessImpl`'s reaper) only exists on the
/// Unix `forkAndExec` path.
#[cfg(not(unix))]
fn pipe_available<T>(_pipe: &T) -> usize {
    0
}

pub struct FileDescriptorTable {
    entries: RwLock<FxHashMap<FdId, Arc<FileEntry>>>,
    next_fd: AtomicU32,
    /// Per-fd blocking mode as last requested through [`tcp_set_nonblocking`]
    /// / [`udp_set_nonblocking`].
    ///
    /// [`tcp_set_nonblocking`]: FileDescriptorTable::tcp_set_nonblocking
    /// [`udp_set_nonblocking`]: FileDescriptorTable::udp_set_nonblocking
    ///
    /// The socket itself is the authority on its mode, but `std` exposes no
    /// getter for it, and the close-aware read/write/accept paths below MUST
    /// know: a non-blocking socket has to keep answering `WouldBlock`
    /// immediately (that is the JDK's `IOStatus.UNAVAILABLE` protocol, which
    /// every selector-driven reactor depends on), while only a blocking one may
    /// park in the poll loop. Entries are RETAINED after being applied — they
    /// are the record of the mode, not a one-shot — and dropped on
    /// [`close`](FileDescriptorTable::close).
    ///
    /// An unknown fd answers "blocking", matching both `std`'s default for a
    /// freshly opened socket and `native-io::net::net_fd_is_nonblocking`'s
    /// deliberate choice: guessing "cannot park" for a socket that can is the
    /// unsafe direction, because it drops the close-aware loop around a wait
    /// that really is unbounded.
    nonblocking: RwLock<FxHashMap<FdId, bool>>,
}

impl FileDescriptorTable {
    pub fn new() -> Self {
        let mut entries: FxHashMap<FdId, Arc<FileEntry>> = FxHashMap::default();
        entries.insert(0, Arc::new(FileEntry::Stdin(Mutex::new(io::stdin()))));
        entries.insert(1, Arc::new(FileEntry::Stdout(Mutex::new(io::stdout()))));
        entries.insert(2, Arc::new(FileEntry::Stderr(Mutex::new(io::stderr()))));
        Self {
            entries: RwLock::new(entries),
            next_fd: AtomicU32::new(3),
            nonblocking: RwLock::new(FxHashMap::default()),
        }
    }

    /// Is `fd` still in the table? [`close`](FileDescriptorTable::close)
    /// `remove`s the entry, so this flips exactly when Java closed the fd —
    /// which is the whole question a parked read/write/accept has to be able to
    /// ask. Deliberately a fresh read guard per call, never held across a
    /// syscall.
    fn fd_still_registered(&self, fd: FdId) -> bool {
        self.entries.read().contains_key(&fd)
    }

    /// Whether `set_nonblocking(true)` is in effect for `fd`. See the
    /// [`nonblocking`](Self::nonblocking) field for why the answer for an
    /// unknown fd is `false`.
    fn fd_is_nonblocking(&self, fd: FdId) -> bool {
        self.nonblocking.read().get(&fd).copied().unwrap_or(false)
    }

    fn record_blocking_mode(&self, fd: FdId, nonblocking: bool) {
        self.nonblocking.write().insert(fd, nonblocking);
    }

    /// Park until `sock` is ready, the fd is closed, or `deadline` expires.
    ///
    /// This is the one close-aware wait shared by [`tcp_read`], [`tcp_write`],
    /// [`tcp_accept`] and [`udp_recv`] below — the same shape as the three
    /// readers that landed on 2026-08-07/11 (`net_read_close_aware`,
    /// `socket_channel::read_close_aware`, `net_phase_e::re1_read_close_aware`):
    /// park in `poll`, not in the syscall, and re-ask the registry every
    /// [`FD_CLOSE_POLL_MS`].
    ///
    /// [`tcp_read`]: FileDescriptorTable::tcp_read
    /// [`tcp_write`]: FileDescriptorTable::tcp_write
    /// [`tcp_accept`]: FileDescriptorTable::tcp_accept
    /// [`udp_recv`]: FileDescriptorTable::udp_recv
    ///
    /// # Why a plain blocking syscall could not observe the close
    ///
    /// [`close`](FileDescriptorTable::close) removes the entry from the table
    /// and lets `Drop` shut the OS handle *when the `Arc` count reaches zero* —
    /// and it never does while a reader is parked, because the reader cloned
    /// that `Arc` out through `get_entry` before it started. So the OS handle
    /// stays open and the syscall stays parked. HotSpot's answer to the same
    /// situation is to take the descriptor away underneath the call
    /// (`closesocket` on Windows, `dup2` of a pre-closed fd plus a signal on
    /// Unix); neither is expressible over a shared `Arc` without a
    /// use-after-close the moment the OS recycles the handle number.
    ///
    /// # Returns
    ///
    /// * `Ok(true)` — ready; issue the syscall.
    /// * `Ok(false)` — no poll primitive on this target; the caller must fall
    ///   back to ONE plain blocking syscall (which cannot see the close, but at
    ///   least still transfers).
    /// * `Err(Interrupted)` — the fd was closed from another thread.
    /// * `Err(TimedOut)` — `deadline` expired. The caller maps this to the
    ///   `SocketTimeoutException` its own surface specifies.
    ///
    /// # On expiry
    ///
    /// The wait is bounded twice over, and neither bound is silent. The
    /// per-pass `FD_CLOSE_POLL_MS` slice expiring is NOT an outcome — it is
    /// only the point at which the registry is re-asked, and the loop
    /// continues. The caller's `deadline` expiring IS an outcome, and it ends
    /// the wait with `TimedOut` rather than leaving anything running. A bounded
    /// wait that does not actually end the wait would have fixed the hang on
    /// `close()` and introduced a new one on `SO_TIMEOUT`.
    fn wait_ready_close_aware(
        &self,
        fd: FdId,
        raw: socket2_raw::RawHandle,
        want_write: bool,
        deadline: Option<std::time::Instant>,
    ) -> Result<bool, io::Error> {
        loop {
            let slice = match deadline {
                Some(end) => {
                    let now = std::time::Instant::now();
                    if now >= end {
                        return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
                    }
                    let remaining = end.saturating_duration_since(now).as_millis();
                    (remaining.min(FD_CLOSE_POLL_MS as u128)) as i32
                }
                None => FD_CLOSE_POLL_MS,
            };
            let Some((readable, writable)) = poll_socket_readiness_timeout(raw, slice) else {
                // No usable poll primitive: the pre-close-awareness behaviour.
                return Ok(false);
            };
            // Asked AFTER the poll, so a close that lands while we are parked
            // is seen on the very next pass, and a close that raced a readiness
            // edge still wins — HotSpot fails an I/O that a concurrent
            // `close()` beat, it does not hand back bytes on a closed socket.
            if !self.fd_still_registered(fd) {
                return Err(fd_async_closed_err());
            }
            if want_write && writable {
                return Ok(true);
            }
            if !want_write && readable {
                return Ok(true);
            }
        }
    }

    /// AUDIT 2026-05-16 (B3/B4): clone the `Arc<FileEntry>` for `fd` and
    /// drop the table-level `RwLock` read guard before the caller starts
    /// any blocking I/O on the entry. Returns `None` if the fd is absent.
    fn get_entry(&self, fd: FdId) -> Option<Arc<FileEntry>> {
        self.entries.read().get(&fd).cloned()
    }

    /// Allocate a fresh fd from the counter, guarding against wraparound.
    ///
    /// `Result`-returning openers (`open_read`, `open_write`, …) check the
    /// `u32::MAX - 16` ceiling themselves and surface an `io::Error`. The
    /// infallible `insert_*` / `open_pipe` paths cannot return an error, so
    /// they call this helper: on a (practically unreachable, ~4-billion-fd)
    /// overflow it aborts the process rather than letting the counter wrap
    /// and alias the reserved stdin/stdout/stderr fds (0/1/2) — silent fd
    /// aliasing would corrupt unrelated streams. `abort` is used instead of
    /// `panic!` so the failure cannot unwind through JIT/native frames.
    fn alloc_fd_or_abort(&self) -> FdId {
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            eprintln!("fatal: file descriptor counter overflow — aborting to avoid aliasing stdin/stdout/stderr");
            std::process::abort();
        }
        fd
    }

    /// Open a file for reading. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling — this function
    /// deliberately does not enforce that, to avoid duplicating (and
    /// potentially conflicting with) the VM's own sandbox policy.
    pub fn open_read(&self, path: &str) -> Result<FdId, io::Error> {
        // Reserve fd first, before opening the file.
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::File::open(path)?;
        let reader = BufReader::new(file);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileRead(Mutex::new(reader))));
        Ok(fd)
    }

    /// Open a file for writing (optionally appending). Returns the fd_id.
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling — this function
    /// deliberately does not enforce that, to avoid duplicating (and
    /// potentially conflicting with) the VM's own sandbox policy.
    pub fn open_write(&self, path: &str, append: bool) -> Result<FdId, io::Error> {
        // Reserve fd first, before opening the file.
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(!append)
            .append(append)
            .open(path)?;
        let writer = BufWriter::new(file);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileWrite(Mutex::new(writer))));
        Ok(fd)
    }

    /// Read a single byte. Returns 0-255 or -1 at EOF.
    pub fn read_byte(&self, fd: FdId) -> Result<i32, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdin(stdin) => {
                let mut buf = [0u8; 1];
                let n = stdin.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::FileRead(reader) => {
                let mut buf = [0u8; 1];
                let n = reader.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            // WP1.12 — subprocess stdout/stderr read.
            FileEntry::ChildStdoutPipe(p) => {
                let mut buf = [0u8; 1];
                let n = p.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::ChildStderrPipe(p) => {
                let mut buf = [0u8; 1];
                let n = p.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::ChildMergedPipe(p) => {
                let mut buf = [0u8; 1];
                let n = p.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            FileEntry::FileReadWrite(file) => {
                let mut buf = [0u8; 1];
                let n = file.lock().read(&mut buf)?;
                Ok(if n == 0 { -1 } else { buf[0] as i32 })
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Read up to `len` bytes into `buf`. Returns count read, or 0 at EOF.
    pub fn read_bytes(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdin(stdin) => {
                let n = stdin.lock().read(buf)?;
                Ok(n)
            }
            FileEntry::FileRead(reader) => reader.lock().read(buf),
            // WP1.12 — subprocess stdout/stderr bulk read.
            FileEntry::ChildStdoutPipe(p) => p.lock().read(buf),
            FileEntry::ChildStderrPipe(p) => p.lock().read(buf),
            FileEntry::ChildMergedPipe(p) => p.lock().read(buf),
            // Read+write file (FileChannel via newFileChannel /
            // RandomAccessFile) — the real `FileDispatcherImpl.read0` path
            // reaches here. Read at the current cursor, matching `rw_read`.
            FileEntry::FileReadWrite(file) => file.lock().read(buf),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Read one line (for BufferedReader). Returns None at EOF.
    /// Read a single line from the file descriptor, recognizing any of
    /// the three line terminators `\n`, `\r\n`, or a bare `\r` — matching
    /// `java.io.BufferedReader.readLine` exactly (JDK 25 javadoc:
    /// "A line is considered to be terminated by any one of a line feed
    /// ('\n'), a carriage return ('\r'), a carriage return followed
    /// immediately by a line feed, or by reaching the end-of-file (EOF)").
    ///
    /// Previously this delegated to `std::io::BufRead::read_line` which
    /// only terminates on `\n`, so a file written on classic Mac OS
    /// (`\r`-only line endings) would read as a single gigantic line.
    /// T2.4.14 asked for byte-level correctness; the new implementation
    /// reads one byte at a time from the underlying stream and stops
    /// at the first terminator, consuming `\n` after `\r` if present to
    /// avoid leaking it into the next call.
    ///
    /// Returns `Ok(None)` on EOF (no bytes read), `Ok(Some(line))`
    /// otherwise. The trailing terminator is *not* included in the
    /// returned string. I/O errors propagate as `Err`.
    pub fn read_line(&self, fd: FdId) -> Result<Option<String>, io::Error> {
        // Helper that reads a single byte from a `BufRead` source and
        // returns `None` at EOF. We cannot use `read_exact([u8; 1])`
        // because EOF is a normal termination, not an error.
        fn read_one<R: io::BufRead>(r: &mut R) -> io::Result<Option<u8>> {
            let mut buf = [0u8; 1];
            match r.read(&mut buf)? {
                0 => Ok(None),
                _ => Ok(Some(buf[0])),
            }
        }
        // Loop that reads bytes from a generic `BufRead` until the
        // first line terminator. After seeing `\r`, peek the next
        // byte and consume it iff it is `\n` (so `\r\n` stays
        // atomic). Otherwise fall through.
        fn read_line_inner<R: io::BufRead>(r: &mut R) -> io::Result<Option<String>> {
            let mut bytes: Vec<u8> = Vec::new();
            loop {
                match read_one(r)? {
                    None => {
                        if bytes.is_empty() {
                            return Ok(None);
                        } else {
                            break;
                        }
                    }
                    Some(b'\n') => break,
                    Some(b'\r') => {
                        // Peek the next byte: if it's \n, consume it.
                        let peek = r.fill_buf()?;
                        if !peek.is_empty() && peek[0] == b'\n' {
                            r.consume(1);
                        }
                        break;
                    }
                    Some(b) => bytes.push(b),
                }
            }
            // Decode as UTF-8 lossily — matching JDK behavior where a
            // reader atop a charset will have already transcoded, but
            // our line reader operates at the byte level.
            Ok(Some(String::from_utf8_lossy(&bytes).into_owned()))
        }

        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdin(stdin) => {
                // `io::Stdin` itself is not `BufRead`, but `StdinLock`
                // — obtained via `Stdin::lock()` — is. The outer
                // parking_lot `Mutex` guard lets us hold the
                // `StdinLock` exclusively while we read one line.
                let guard = stdin.lock();
                let mut stdin_lock = guard.lock();
                read_line_inner(&mut stdin_lock)
            }
            FileEntry::FileRead(reader) => {
                // `BufReader<File>` implements `BufRead`; the
                // parking_lot guard defers directly.
                let mut guard = reader.lock();
                read_line_inner(&mut *guard)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Write a single byte.
    pub fn write_byte(&self, fd: FdId, b: u8) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdout(stdout) => {
                let _stdio_guard = stdio_write_lock().lock();
                stdout.lock().write_all(&[b])?;
                Ok(())
            }
            FileEntry::Stderr(stderr) => {
                let _stdio_guard = stdio_write_lock().lock();
                stderr.lock().write_all(&[b])?;
                Ok(())
            }
            FileEntry::FileWrite(writer) => {
                writer.lock().write_all(&[b])?;
                Ok(())
            }
            // WP1.12 — write to subprocess stdin.
            FileEntry::ChildStdinPipe(p) => {
                p.lock().write_all(&[b])?;
                Ok(())
            }
            FileEntry::FileReadWrite(file) => {
                file.lock().write_all(&[b])?;
                Ok(())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Write bytes from a slice.
    pub fn write_bytes(&self, fd: FdId, data: &[u8]) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::Stdout(stdout) => {
                let _stdio_guard = stdio_write_lock().lock();
                stdout.lock().write_all(data)?;
                Ok(())
            }
            FileEntry::Stderr(stderr) => {
                let _stdio_guard = stdio_write_lock().lock();
                stderr.lock().write_all(data)?;
                Ok(())
            }
            FileEntry::FileWrite(writer) => {
                // Java's `FileOutputStream.write` is UNBUFFERED — each write
                // reaches the OS immediately and is visible to a concurrent
                // reader on a separate handle. Our `BufWriter` would otherwise
                // hold the bytes in userspace, so a second open of the same
                // path reads an empty/partial file (DaCapo luindex's
                // `FileDigest` re-reads `stdout.log` without first flushing the
                // writer → empty-input SHA-1, digest validation fails). Flush
                // after the write to restore the immediate-visibility
                // contract; apps that want batching use `BufferedOutputStream`
                // (Java-side), which hands us already-coalesced chunks — so
                // this matches HotSpot, whose FOS issues a `write()` syscall
                // per call too.
                let mut w = writer.lock();
                w.write_all(data)?;
                w.flush()?;
                Ok(())
            }
            // WP1.12 — bulk write to subprocess stdin.
            FileEntry::ChildStdinPipe(p) => {
                p.lock().write_all(data)?;
                Ok(())
            }
            // A read+write file (FileChannel via newFileChannel /
            // RandomAccessFile) — the real `FileDispatcherImpl.write0` path
            // reaches here. Write at the file's current cursor (unbuffered
            // fs::File), matching `rw_write`. Without this arm the generic
            // write path fell to "bad fd" even though the fd is valid.
            FileEntry::FileReadWrite(file) => {
                file.lock().write_all(data)?;
                Ok(())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    /// Write a UTF-8 string.
    pub fn write_string(&self, fd: FdId, text: &str) -> Result<(), io::Error> {
        self.write_bytes(fd, text.as_bytes())
    }

    /// Flush buffered output.
    pub fn flush(&self, fd: FdId) -> Result<(), io::Error> {
        // No fd present is a no-op (consistent with previous behavior
        // for non-writable / nonexistent fds).
        let Some(entry) = self.get_entry(fd) else {
            return Ok(());
        };
        match &*entry {
            FileEntry::Stdout(stdout) => {
                let _stdio_guard = stdio_write_lock().lock();
                stdout.lock().flush()?;
                Ok(())
            }
            FileEntry::Stderr(stderr) => {
                let _stdio_guard = stdio_write_lock().lock();
                stderr.lock().flush()?;
                Ok(())
            }
            FileEntry::FileWrite(writer) => {
                writer.lock().flush()?;
                Ok(())
            }
            // WP1.12 — flushing ChildStdin pushes buffered bytes into the
            // subprocess's stdin immediately. Without this, the child may
            // block waiting for data that's sitting in our userspace buffer.
            FileEntry::ChildStdinPipe(p) => {
                p.lock().flush()?;
                Ok(())
            }
            _ => Ok(()), // no-op for non-writable fds
        }
    }

    /// Close a file descriptor. Returns an error if flushing a writer fails.
    ///
    /// AUDIT 2026-05-16 (B3/B4 + B7): the entry is removed from the table
    /// *first*, releasing the write guard before any blocking I/O (flush)
    /// runs. The previous implementation held the write guard across
    /// `writer.lock().flush()?` — a slow flush would block every other
    /// fd-table mutation VM-wide. The previous code also leaked the
    /// entry on flush error: it propagated the `?` while the entry
    /// was still in the table, so the caller could neither retry nor
    /// release the resource. Now we always remove the entry on close
    /// and flush the dropped copy out-of-band, returning the flush
    /// error if any.
    pub fn close(&self, fd: FdId) -> Result<(), io::Error> {
        // Don't close stdin/stdout/stderr
        if fd < 3 {
            return Ok(());
        }
        // Remove first (under the write guard), then drop the guard
        // before performing any blocking I/O on the removed entry.
        let removed = {
            let mut entries = self.entries.write();
            entries.remove(&fd)
        };
        // Drop the recorded blocking mode with the fd. Taken AFTER the entry
        // removal so a reader parked in `wait_ready_close_aware` can never see
        // the mode disappear before the registry answer it actually keys on.
        self.nonblocking.write().remove(&fd);
        let Some(entry) = removed else {
            return Ok(());
        };
        // Flush writer-style entries out of the table lock. We can't
        // move out of an Arc (other clones could still exist
        // theoretically — though in practice they shouldn't), so we
        // flush through the inner Mutex on the existing Arc and let
        // `Drop` close the OS handle when the Arc count reaches zero.
        //
        // `FileReadWrite`/`FileRead` also need the lock acquired (and
        // dropped) here, even though there's nothing to flush: a reader
        // or writer that already cloned this Arc via `get_entry` before
        // the `remove()` above (e.g. a `pwrite_at`/`rw_read` call in
        // flight on another thread) is invisible to the table lock and
        // would otherwise keep running concurrently with — or after —
        // this close(). Acquiring the entry's own lock blocks until that
        // in-flight operation finishes, so no read/write started before
        // this close() call can still be touching the file once it
        // returns (callers that skip joining a writer thread before
        // closing, e.g. H2 MVStore's `FileStore.stopBackgroundThread
        // (waitForIt=false)`, otherwise race a stray write against the
        // file being closed/truncated/reopened).
        match &*entry {
            FileEntry::FileWrite(writer) => writer.lock().flush(),
            FileEntry::ChildStdinPipe(p) => p.lock().flush(),
            FileEntry::FileReadWrite(file) => {
                let _ = file.lock();
                Ok(())
            }
            FileEntry::FileRead(reader) => {
                let _ = reader.lock();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// Estimate available bytes (best-effort).
    pub fn available(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd"))?;
        match &*entry {
            FileEntry::FileRead(reader) => {
                let mut buf = reader.lock();
                let buffered = buf.buffer().len();
                // Also account for bytes remaining in the underlying file
                // beyond what's already buffered.
                let file_remaining = {
                    use std::io::Seek;
                    let inner = buf.get_mut();
                    let pos = inner.stream_position().unwrap_or(0);
                    let end = inner.seek(std::io::SeekFrom::End(0)).unwrap_or(0);
                    // Seek back to where we were
                    let _ = inner.seek(std::io::SeekFrom::Start(pos));
                    end.saturating_sub(pos) as usize
                };
                Ok(buffered + file_remaining)
            }
            // Subprocess pipes. Answering 0 here is not "no data" — it is a
            // wrong answer that destroys data, because
            // `ProcessImpl$ProcessPipeInputStream.processExited()` drains the
            // pipe with `while ((j = in.available()) > 0)` and then CLOSES it,
            // installing whatever it drained as the stream's new source. A 0
            // makes the drain loop exit immediately, so the child's output is
            // replaced by `ProcessBuilder.NullInputStream.INSTANCE` and the
            // application reads EOF from a child that printed perfectly well.
            //
            // The reaper thread runs that method the moment the child exits, so
            // the shorter the child, the likelier it wins the race against the
            // application's first read. Measured on `sh -c 'echo out-line'`,
            // which lost its output every time.
            FileEntry::ChildStdoutPipe(p) => Ok(pipe_available(&*p.lock())),
            FileEntry::ChildStderrPipe(p) => Ok(pipe_available(&*p.lock())),
            FileEntry::ChildMergedPipe(p) => Ok(pipe_available(&*p.lock())),
            FileEntry::Stdin(_) => Ok(0),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd")),
        }
    }

    // -----------------------------------------------------------------------
    // Phase 92: AsynchronousFileChannel, DatagramChannel support
    // -----------------------------------------------------------------------

    /// Open a file for read+write access (used by AsynchronousFileChannel).
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling — this function
    /// deliberately does not enforce that, to avoid duplicating (and
    /// potentially conflicting with) the VM's own sandbox policy.
    pub fn open_read_write(&self, path: &str, create: bool) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(create)
            .open(path)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileReadWrite(Mutex::new(file))));
        Ok(fd)
    }

    /// Open a file for `java.io.RandomAccessFile`. Always produces a
    /// `FileReadWrite` entry so the RAF read/write/seek natives **and** any
    /// `FileChannel` derived from the RAF (`raf.getChannel()`) resolve to the
    /// **same** fd — they share one `FileDescriptor`, and HotSpot shares one
    /// OS fd between them. `write == false` mirrors mode `"r"` (open the file
    /// read-only: do not create it, and let a stray write fail at the OS
    /// level, matching a read-only RAF). `write == true` mirrors `"rw"`/
    /// `"rws"`/`"rwd"` (read+write, create if absent). The caller emulates the
    /// `"rws"`/`"rwd"` per-write sync via [`Self::rw_sync`].
    ///
    /// # Security
    ///
    /// `path` is passed **verbatim** to the OS with no path-traversal or
    /// sandbox check performed here. When the value originates from
    /// Java-controlled input, the caller (the VM / native-io sandbox layer)
    /// MUST sanitize it for path traversal before calling.
    pub fn open_random_access(&self, path: &str, write: bool) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let file = fs::OpenOptions::new()
            .read(true)
            .write(write)
            .create(write)
            .open(path)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::FileReadWrite(Mutex::new(file))));
        Ok(fd)
    }

    /// Read bytes from a file at a specific position (pread).
    /// Does not change the file's current position.
    pub fn pread_at(&self, fd: FdId, buf: &mut [u8], position: u64) -> Result<usize, io::Error> {
        use io::{Read, Seek, SeekFrom};
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pread"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                let saved = f.stream_position()?;
                f.seek(SeekFrom::Start(position))?;
                let n = f.read(buf)?;
                f.seek(SeekFrom::Start(saved))?;
                Ok(n)
            }
            FileEntry::FileRead(reader) => {
                // Seek the `BufReader` itself (not the file behind its
                // back). `BufReader`'s `Seek` impl reconciles or discards
                // its internal buffer, and `stream_position()` returns
                // the logical position accounting for buffered-but-
                // unconsumed bytes — so saving and restoring it leaves a
                // subsequent sequential `read()` returning the correct
                // bytes. Seeking the inner file directly would leave the
                // buffer stale and corrupt the next sequential read.
                let mut r = reader.lock();
                let saved = r.stream_position()?;
                r.seek(SeekFrom::Start(position))?;
                let n = r.read(buf)?;
                r.seek(SeekFrom::Start(saved))?;
                Ok(n)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for pread")),
        }
    }

    /// Write bytes to a file at a specific position (pwrite).
    /// Does not change the file's current position.
    pub fn pwrite_at(&self, fd: FdId, data: &[u8], position: u64) -> Result<usize, io::Error> {
        use io::{Seek, SeekFrom, Write};
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pwrite"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                let saved = f.stream_position()?;
                f.seek(SeekFrom::Start(position))?;
                f.write_all(data)?;
                f.seek(SeekFrom::Start(saved))?;
                Ok(data.len())
            }
            FileEntry::FileWrite(writer) => {
                // Seek the `BufWriter` itself, not the file behind its
                // back. `BufWriter`'s `Seek` impl flushes any pending
                // buffered bytes to their original offset *before*
                // moving the cursor — so the positional write below
                // cannot interleave with stale buffer contents, and the
                // restore-seek leaves a subsequent sequential write
                // appending at the correct offset. Seeking the inner
                // file directly would strand un-flushed bytes and write
                // them at the wrong offset on the next flush.
                let mut w = writer.lock();
                let saved = w.stream_position()?;
                w.seek(SeekFrom::Start(position))?;
                w.write_all(data)?;
                w.flush()?;
                w.seek(SeekFrom::Start(saved))?;
                Ok(data.len())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for pwrite")),
        }
    }

    /// Run `f` against the `Seek` view of **any** file-backed entry.
    ///
    /// All three file variants are seekable, but two of them are wrapped in a
    /// buffer, so we hand `f` the *wrapper* rather than the inner `fs::File`:
    /// `BufReader`'s `Seek` impl reconciles/discards its read-ahead buffer and
    /// its `stream_position` subtracts the unconsumed remainder, while
    /// `BufWriter`'s flushes pending bytes at their original offset *before*
    /// moving the cursor and its `stream_position` adds the pending length.
    /// Seeking the file behind either wrapper's back would strand the buffer —
    /// a reader would keep serving pre-seek bytes, a writer would flush its
    /// pending bytes at the new offset. (`pread_at`/`pwrite_at` already work
    /// this way; see their comments.)
    ///
    /// Accepting only `FileReadWrite` here is what produced
    /// `IOException: seek0: bad fd for rw_seek` for every `FileChannel`
    /// obtained from a `FileInputStream`/`FileOutputStream` — those register as
    /// `FileRead`/`FileWrite`, and on Windows `FileChannelImpl.transferToDirect`
    /// brackets each transfer with `position()`/`position(pos)`
    /// (`transferToDirectlyNeedsPositionLock()` is `true` there), so
    /// `ExpandWar.copy`'s `ic.transferTo(pos, size, oc)` threw on its first
    /// call. Keep every seek/position/size accessor below routed through this
    /// helper so the variant list cannot drift apart again.
    fn with_seekable<R>(
        &self,
        fd: FdId,
        what: &'static str,
        f: impl FnOnce(&mut dyn io::Seek) -> Result<R, io::Error>,
    ) -> Result<R, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("bad fd for {what}")))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => f(&mut *file.lock()),
            FileEntry::FileRead(reader) => f(&mut *reader.lock()),
            FileEntry::FileWrite(writer) => f(&mut *writer.lock()),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("bad fd for {what}"),
            )),
        }
    }

    /// Sequential read from a readable file entry (advances the cursor).
    pub fn rw_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_read"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                f.read(buf)
            }
            // A `FileInputStream`-backed channel. Read *through* the
            // `BufReader` (as `read_bytes` does) so a preceding `rw_seek`,
            // which moved the wrapper and not the inner file, is honoured.
            FileEntry::FileRead(reader) => reader.lock().read(buf),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_read",
            )),
        }
    }

    /// Sequential write to a writable file entry (advances the cursor).
    pub fn rw_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        use io::Write;
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_write"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                f.write(data)
            }
            // A `FileOutputStream`-backed channel. Write through the
            // `BufWriter` and flush, matching `write_bytes`'s
            // immediate-visibility contract (Java's `FileOutputStream.write`
            // is unbuffered) and keeping the wrapper's cursor authoritative.
            FileEntry::FileWrite(writer) => {
                let mut w = writer.lock();
                let n = w.write(data)?;
                w.flush()?;
                Ok(n)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_write",
            )),
        }
    }

    /// Seek in a file-backed entry. Returns the new position.
    pub fn rw_seek(&self, fd: FdId, pos: io::SeekFrom) -> Result<u64, io::Error> {
        use io::Seek;
        self.with_seekable(fd, "rw_seek", |s| s.seek(pos))
    }

    /// Get the current position in a file-backed entry.
    pub fn rw_position(&self, fd: FdId) -> Result<u64, io::Error> {
        use io::Seek;
        self.with_seekable(fd, "rw_position", |s| s.stream_position())
    }

    /// Clone the underlying `std::fs::File` for any file-backed entry.
    ///
    /// The returned handle refers to the same kernel file but has an
    /// independent seek cursor, which is what `mmap` / `MapViewOfFile`
    /// require. Returns an error if the fd is not backed by a real file.
    pub fn clone_file(&self, fd: FdId) -> Result<fs::File, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for clone_file"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => file.lock().try_clone(),
            FileEntry::FileRead(reader) => reader.lock().get_ref().try_clone(),
            FileEntry::FileWrite(writer) => {
                // BufWriter::get_ref() borrows; flush first to not lose data.
                let mut w = writer.lock();
                w.flush().ok();
                w.get_ref().try_clone()
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for clone_file",
            )),
        }
    }

    /// Set the length of a writable file entry (truncate or extend).
    pub fn rw_set_length(&self, fd: FdId, len: u64) -> Result<(), io::Error> {
        use io::Write;
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_set_length"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let f = file.lock();
                f.set_len(len)
            }
            // `FileChannel.truncate()` on a `FileOutputStream`-backed channel.
            // Flush first: buffered bytes belong to the pre-truncation file, so
            // letting them land afterwards would re-extend it behind the
            // caller's back. The JDK adjusts the channel position itself after
            // a truncate, so we only resize here.
            FileEntry::FileWrite(writer) => {
                let mut w = writer.lock();
                w.flush()?;
                w.get_ref().set_len(len)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_set_length",
            )),
        }
    }

    /// Flush a file entry's contents to stable storage. Used by
    /// `RandomAccessFile` modes `"rws"` (`data_only == false` → `sync_all`,
    /// data + metadata) and `"rwd"` (`data_only == true` → `sync_data`, data
    /// only) which require every write to reach durable storage before the
    /// call returns, and by `FileDescriptor.sync()` — which Java allows on the
    /// descriptor of a plain `FileOutputStream`, hence the `FileWrite` arm.
    pub fn rw_sync(&self, fd: FdId, data_only: bool) -> Result<(), io::Error> {
        use io::Write;
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for rw_sync"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let f = file.lock();
                if data_only {
                    f.sync_data()
                } else {
                    f.sync_all()
                }
            }
            FileEntry::FileWrite(writer) => {
                // Buffered bytes are not "written" as far as the kernel is
                // concerned, so they must reach it before we ask for the sync.
                let mut w = writer.lock();
                w.flush()?;
                if data_only {
                    w.get_ref().sync_data()
                } else {
                    w.get_ref().sync_all()
                }
            }
            // No `FileRead` arm on purpose: a read-only handle has nothing to
            // flush, and `FlushFileBuffers` on one fails with
            // ERROR_ACCESS_DENIED on Windows (it needs GENERIC_WRITE), so
            // "succeeding" here would mean lying on one platform and erroring
            // on the other.
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for rw_sync",
            )),
        }
    }

    /// Get the size of a file by fd.
    pub fn file_size(&self, fd: FdId) -> Result<u64, io::Error> {
        use io::{Seek, SeekFrom};
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for size"))?;
        match &*entry {
            FileEntry::FileReadWrite(file) => {
                let mut f = file.lock();
                let saved = f.stream_position()?;
                let size = f.seek(SeekFrom::End(0))?;
                f.seek(SeekFrom::Start(saved))?;
                Ok(size)
            }
            FileEntry::FileRead(reader) => {
                let mut r = reader.lock();
                let inner = r.get_mut();
                let saved = inner.stream_position()?;
                let size = inner.seek(SeekFrom::End(0))?;
                inner.seek(SeekFrom::Start(saved))?;
                Ok(size)
            }
            // `FileChannel.size()` on a `FileOutputStream`-backed channel —
            // also the `position()` path for an *append*-mode channel, which
            // the JDK answers with `nd.size(fd)` rather than a seek. Flush
            // first so buffered-but-unwritten bytes count toward the size, then
            // measure the inner file and restore its cursor (the `BufWriter`'s
            // own position is derived from it).
            FileEntry::FileWrite(writer) => {
                use io::Write;
                let mut w = writer.lock();
                w.flush()?;
                let inner = w.get_mut();
                let saved = inner.stream_position()?;
                let size = inner.seek(SeekFrom::End(0))?;
                inner.seek(SeekFrom::Start(saved))?;
                Ok(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for size")),
        }
    }

    /// Open a UDP socket. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `bind_addr` is passed **verbatim** to the OS with no sandbox/SSRF
    /// check performed here. When the value originates from Java-controlled
    /// input, the caller (the VM / native-io sandbox layer) MUST sanitize it
    /// before calling — this function deliberately does not enforce that, to
    /// avoid duplicating (and potentially conflicting with) the VM's own
    /// sandbox policy.
    /// Open a **dual-stack** UDP socket on an ephemeral port, the way
    /// `DatagramChannel.open()` does on HotSpot. See
    /// [`open_udp_dual_stack_socket`] for why one family is not enough.
    ///
    /// Deliberately separate from [`open_udp`]: `java.net.DatagramSocket` and
    /// `MulticastSocket` reach that one with an explicit address far more
    /// often, and a multicast join in particular is family-specific. This
    /// keeps the change to the caller that measurably needed it.
    pub fn open_udp_dual_stack(&self) -> Result<FdId, io::Error> {
        self.open_udp_dual_stack_port(0)
    }

    /// [`open_udp_dual_stack`](Self::open_udp_dual_stack) on an explicit port.
    pub fn open_udp_dual_stack_port(&self, port: u16) -> Result<FdId, io::Error> {
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let socket = open_udp_dual_stack_socket(port)?;
        disable_udp_connreset(&socket);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(socket)));
        Ok(fd)
    }

    pub fn open_udp(&self, bind_addr: Option<&str>) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let addr = bind_addr.unwrap_or("0.0.0.0:0");
        let socket = std::net::UdpSocket::bind(addr)?;
        disable_udp_connreset(&socket);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(socket)));
        Ok(fd)
    }

    /// Open a UDP socket with `SO_REUSEADDR` set **before** bind. Returns the fd_id.
    ///
    /// `java.net.MulticastSocket` enables `SO_REUSEADDR` prior to binding so that
    /// multiple receivers (and repeated bind/close cycles within one process)
    /// can share a multicast group port. Plain `open_udp` binds via
    /// `std::net::UdpSocket::bind`, which does not set the option — on Windows
    /// that surfaces as `WSAEADDRINUSE` (os error 10048) when a recently-closed
    /// or concurrently-held port is re-bound. This variant mirrors the JDK
    /// semantics using `socket2` (create → set_reuse_address → bind).
    ///
    /// # Security
    ///
    /// Same `bind_addr` caveat as [`open_udp`]: the address is passed verbatim
    /// to the OS with no sandbox/SSRF check here.
    pub fn open_udp_reuse(&self, bind_addr: Option<&str>) -> Result<FdId, io::Error> {
        use std::net::ToSocketAddrs;
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let addr_str = bind_addr.unwrap_or("0.0.0.0:0");
        let sock_addr = addr_str
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addr resolved"))?;
        let domain = if sock_addr.is_ipv4() {
            socket2::Domain::IPV4
        } else {
            socket2::Domain::IPV6
        };
        let socket =
            socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
        socket.set_reuse_address(true)?;
        socket.bind(&sock_addr.into())?;
        let udp: std::net::UdpSocket = socket.into();
        disable_udp_connreset(&udp);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(udp)));
        Ok(fd)
    }

    /// Rebind an open UDP socket to the **dual-stack wildcard**, keeping its
    /// `FdId`.
    ///
    /// `DatagramChannel.bind(null)` — and `bind(new InetSocketAddress(0))`,
    /// whose address is the v4 `0.0.0.0` — are wildcard binds, and on HotSpot
    /// they leave a dual-stack channel dual-stack: measured on JDK 25,
    /// `DatagramChannel.open().bind(null).getLocalAddress()` is
    /// `/[0:0:0:0:0:0:0:0]:port`, and so is the `"0.0.0.0"` form.
    ///
    /// Routing those through [`udp_rebind`] with the literal `"0.0.0.0:0"`
    /// replaced the AF_INET6 socket [`open_udp_dual_stack`] had just created
    /// with an AF_INET one, so the channel lost the second family the moment
    /// it was bound — which is every netty datagram channel, because
    /// `AbstractBootstrap` binds before use. The visible symptom was the one
    /// `open_udp_dual_stack_socket` documents: a send to `::1` failing with
    /// `EAFNOSUPPORT` on a channel that had been opened dual-stack.
    pub fn udp_rebind_dual_stack(
        &self,
        fd: FdId,
        port: u16,
        reuse_address: bool,
    ) -> Result<(), io::Error> {
        match self.get_entry(fd) {
            Some(entry) if matches!(&*entry, FileEntry::UdpSocket(_)) => {}
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fd is not a UDP socket",
                ))
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "bad fd for udp rebind",
                ))
            }
        }
        let udp = if reuse_address {
            let socket = socket2::Socket::new(
                socket2::Domain::IPV6,
                socket2::Type::DGRAM,
                Some(socket2::Protocol::UDP),
            )?;
            socket.set_only_v6(false)?;
            socket.set_reuse_address(true)?;
            let addr = std::net::SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port));
            socket.bind(&addr.into())?;
            std::net::UdpSocket::from(socket)
        } else {
            open_udp_dual_stack_socket(port)?
        };
        disable_udp_connreset(&udp);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(udp)));
        Ok(())
    }

    /// Bind an already-open UDP socket to `bind_addr`, KEEPING ITS `FdId`.
    ///
    /// A bound socket cannot be rebound, so this creates a freshly bound one
    /// and swaps it into the table under the same id. The id is what matters:
    /// it is the identity every side table keys on, and in particular the NIO
    /// selector registers a channel under it — which, for netty, happens
    /// BEFORE the bind (`AbstractChannel.register0` runs `javaChannel()
    /// .register(selector, 0)`, and `doBind` comes later). Allocating a fresh
    /// id here, which is what `close(old)` + `open_udp(addr)` did, left that
    /// registration keyed on an id nothing answers to and holding a dup of a
    /// socket that would never become readable again: netty's event loop
    /// never saw a single inbound datagram.
    ///
    /// The previous socket closes when the last `Arc` to it drops, so a
    /// selector still holding a dup keeps that dup alive until it re-registers
    /// — the epoll set then drops the old fd on its own.
    ///
    /// `reuse_address` reapplies `SO_REUSEADDR`, which is a PRE-bind option:
    /// whatever was set on the socket this one replaces is not inherited.
    pub fn udp_rebind(
        &self,
        fd: FdId,
        bind_addr: Option<&str>,
        reuse_address: bool,
    ) -> Result<(), io::Error> {
        use std::net::ToSocketAddrs;
        // Refuse before creating anything, so a bad fd cannot leak a socket.
        match self.get_entry(fd) {
            Some(entry) if matches!(&*entry, FileEntry::UdpSocket(_)) => {}
            Some(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fd is not a UDP socket",
                ))
            }
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "bad fd for udp rebind",
                ))
            }
        }
        let addr_str = bind_addr.unwrap_or("0.0.0.0:0");
        let udp = if reuse_address {
            let sock_addr = addr_str
                .to_socket_addrs()?
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addr resolved"))?;
            let domain = if sock_addr.is_ipv4() {
                socket2::Domain::IPV4
            } else {
                socket2::Domain::IPV6
            };
            let socket =
                socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
            socket.set_reuse_address(true)?;
            socket.bind(&sock_addr.into())?;
            std::net::UdpSocket::from(socket)
        } else {
            std::net::UdpSocket::bind(addr_str)?
        };
        disable_udp_connreset(&udp);
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::UdpSocket(udp)));
        Ok(())
    }

    /// Send UDP datagram to a target address. Returns bytes sent.
    pub fn udp_send(&self, fd: FdId, data: &[u8], target: &str) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp send"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                // Not `sock.send_to(data, target)`: that resolves the string
                // itself and hands the raw result to the OS, which refuses a
                // v4 sockaddr on a dual-stack v6 socket. See `udp_target_addr`.
                let addr = udp_target_addr(sock, target)?;
                sock.send_to(data, addr)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp send",
            )),
        }
    }

    /// Connect a UDP socket to its peer address.
    ///
    /// A connected datagram socket still uses UDP, but it gains the JDK
    /// `DatagramChannel.write` / `read` contract and rejects datagrams from
    /// other peers at the OS boundary.
    pub fn udp_connect(&self, fd: FdId, target: &str) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp connect"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                let addr = udp_target_addr(sock, target)?;
                sock.connect(addr)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp connect",
            )),
        }
    }

    /// Send a UDP datagram through a connected socket.
    pub fn udp_send_connected(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp write"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => sock.send(data),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp write",
            )),
        }
    }

    /// Clone a UDP socket for selector polling without transferring ownership
    /// from the Java-visible channel.
    pub fn udp_try_clone(&self, fd: FdId) -> Result<std::net::UdpSocket, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp clone"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => sock.try_clone(),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp clone",
            )),
        }
    }

    /// Receive a UDP datagram. Returns (bytes_read, source_addr).
    ///
    /// # Close-awareness (2026-08-12)
    ///
    /// This is the shared chokepoint for every blocking datagram receive in the
    /// VM: `native-io::net`'s `MulticastSocket.receive`, and `native-io::lib`'s
    /// `native_dc_read` / `native_dc_receive`. All three used to park inside
    /// `recv_from` on a socket whose `Arc` they had cloned out of the table,
    /// and none of them could observe another thread's `close()` — the failure
    /// this whole family exists to remove. Fixing the chokepoint fixes all
    /// three at once rather than three times over.
    ///
    /// `SO_RCVTIMEO` stays the first line and is not disturbed: the socket's own
    /// `read_timeout` is read back here and becomes the poll loop's deadline, so
    /// a `DatagramSocket.setSoTimeout` reader still gets `TimedOut` at the same
    /// moment it did before. That matters because the two are different regimes
    /// — `SO_RCVTIMEO` bounds the *syscall*, and on a socket where the syscall
    /// is never issued (because we park in `poll` instead) it would never fire
    /// at all. The deadline is what ends the park where `SO_RCVTIMEO` cannot.
    pub fn udp_recv(&self, fd: FdId, buf: &mut [u8]) -> Result<(usize, String), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp recv"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                // A non-blocking socket must keep answering `WouldBlock`
                // immediately — parking would take the JDK's
                // `IOStatus.UNAVAILABLE` protocol away from its caller.
                if !self.fd_is_nonblocking(fd) {
                    let deadline = sock
                        .read_timeout()
                        .ok()
                        .flatten()
                        .map(|t| std::time::Instant::now() + t);
                    // `Ok(true)` — readable, so the `recv_from` below cannot
                    // park. `Ok(false)` — no poll primitive on this target;
                    // fall through to the plain blocking `recv_from`, which
                    // cannot see the close but at least still transfers.
                    let _ =
                        self.wait_ready_close_aware(fd, raw_handle_of(sock), false, deadline)?;
                }
                let (n, addr) = sock.recv_from(buf)?;
                Ok((n, unmap_v4_mapped(addr).to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for udp recv",
            )),
        }
    }

    /// Set non-blocking mode on a UDP socket.
    pub fn udp_set_nonblocking(&self, fd: FdId, nonblocking: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        let applied = match &*entry {
            FileEntry::UdpSocket(sock) => sock.set_nonblocking(nonblocking),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        };
        // See `tcp_set_nonblocking` — record only what the socket accepted.
        if applied.is_ok() {
            self.record_blocking_mode(fd, nonblocking);
        }
        applied
    }

    /// Get the local address of a UDP socket.
    pub fn udp_local_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(sock) => Ok(unmap_v4_mapped(sock.local_addr()?).to_string()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Connect a TCP stream to a remote address. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `addr` is passed **verbatim** to the OS with no sandbox/SSRF check
    /// performed here. When the value originates from Java-controlled input,
    /// the caller (the VM / native-io sandbox layer) MUST sanitize it before
    /// calling — this function deliberately does not enforce that, to avoid
    /// duplicating (and potentially conflicting with) the VM's own sandbox
    /// policy.
    pub fn open_tcp_connect(&self, addr: &str) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let stream = connect_prefer_ipv4(addr)?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::TcpStream(Mutex::new(stream))));
        Ok(fd)
    }

    /// Wrap an existing TcpStream (e.g. from accept). Returns the fd_id.
    pub fn insert_tcp_stream(&self, stream: std::net::TcpStream) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::TcpStream(Mutex::new(stream))));
        fd
    }

    /// WP1.12 — wrap a subprocess's `ChildStdout` pipe and return the fd_id.
    /// The JDK-side `FileInputStream` wrapping this fd gets bytes from the
    /// child process's standard output.
    pub fn insert_child_stdout(&self, stream: std::process::ChildStdout) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildStdoutPipe(Mutex::new(stream))));
        fd
    }

    /// WP1.12 — wrap a subprocess's `ChildStderr` pipe and return the fd_id.
    pub fn insert_child_stderr(&self, stream: std::process::ChildStderr) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildStderrPipe(Mutex::new(stream))));
        fd
    }

    /// `redirectErrorStream(true)`: wrap the read-end of the single pipe that
    /// the child's stdout AND stderr both write to. Read through the same
    /// `read_bytes`/`read_byte` API as the separate pipes.
    pub fn insert_child_merged(&self, reader: std::io::PipeReader) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildMergedPipe(Mutex::new(reader))));
        fd
    }

    /// WP1.12 — wrap a subprocess's `ChildStdin` pipe and return the fd_id.
    /// Writing bytes to this fd pipes them into the child's standard input.
    pub fn insert_child_stdin(&self, stream: std::process::ChildStdin) -> FdId {
        let fd = self.alloc_fd_or_abort();
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::ChildStdinPipe(Mutex::new(stream))));
        fd
    }

    /// Open a TCP listener bound to the given address. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `addr` is passed **verbatim** to the OS with no sandbox check
    /// performed here. When the value originates from Java-controlled input,
    /// the caller (the VM / native-io sandbox layer) MUST sanitize it before
    /// calling — this function deliberately does not enforce that, to avoid
    /// duplicating (and potentially conflicting with) the VM's own sandbox
    /// policy.
    pub fn open_tcp_listener(&self, addr: &str) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back.
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let listener = std::net::TcpListener::bind(addr)?;
        self.entries.write().insert(
            fd,
            Arc::new(FileEntry::TcpListener {
                listener: Mutex::new(listener),
                pending: Mutex::new(VecDeque::new()),
            }),
        );
        Ok(fd)
    }

    /// Accept a connection on a TCP listener. Returns (new_stream_fd, remote_addr).
    pub fn tcp_accept(&self, fd: FdId) -> Result<(FdId, String), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp accept"))?;
        match &*entry {
            FileEntry::TcpListener { listener, pending } => {
                // Drain any connection that `poll_ready` already accepted
                // off the OS backlog before touching the listener itself.
                let queued = pending.lock().pop_front();
                let (stream, addr) = match queued {
                    Some(conn) => conn,
                    // CLOSE-AWARENESS 2026-08-12: an `accept()` on an idle
                    // listener parks for as long as nobody connects, and
                    // `close()` on another thread cannot end it — the entry is
                    // removed from the table but this thread's `Arc` clone keeps
                    // the listening socket open. Park in `poll` and re-ask the
                    // table instead, the shape `net::net_accept_close_aware` and
                    // `socket_channel::accept_close_aware` have carried since
                    // 2026-05-17. A non-blocking listener is untouched: it must
                    // keep answering `WouldBlock` for the JDK's
                    // `IOStatus.UNAVAILABLE` accept protocol.
                    None if !self.fd_is_nonblocking(fd) => {
                        let raw = raw_handle_of(&*listener.lock());
                        loop {
                            if !self.wait_ready_close_aware(fd, raw, false, None)? {
                                break listener.lock().accept()?;
                            }
                            let l = listener.lock();
                            // Zero-timeout re-ask under the lock: a second
                            // acceptor on this fd could have taken the pending
                            // connection between the poll and this lock, and
                            // calling `accept()` anyway would park in the
                            // syscall — the failure this loop exists to remove.
                            if !poll_socket_readiness_timeout(raw, 0)
                                .map(|(r, _)| r)
                                .unwrap_or(true)
                            {
                                drop(l);
                                continue;
                            }
                            break l.accept()?;
                        }
                    }
                    None => listener.lock().accept()?,
                };
                let new_fd = self.insert_tcp_stream(stream);
                Ok((new_fd, addr.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp accept",
            )),
        }
    }

    /// Read from a TCP stream. Returns bytes read.
    ///
    /// # Close-awareness (2026-08-12)
    ///
    /// Same shape as [`udp_recv`](Self::udp_recv), and the same reason: this is
    /// the chokepoint under `native-builtins`' `SocketChannel.read` /
    /// `Socket.getInputStream().read()` synthetic paths, and none of them could
    /// observe a `close()` from another thread.
    ///
    /// The poll happens **outside** the per-fd `Mutex`, which fixes a second
    /// defect in passing: the old code held that mutex for the whole duration
    /// of a blocking read, so every [`tcp_write`](Self::tcp_write) on the same
    /// fd queued behind a reader waiting on a peer that might never speak. That
    /// is a per-fd half-duplex wedge, and `try_clone_tcp` below exists only
    /// because of it. The lock is now taken only once the socket is already
    /// readable.
    pub fn tcp_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp read"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => {
                if self.fd_is_nonblocking(fd) {
                    let mut s = stream.lock();
                    return s.read(buf);
                }
                let (raw, deadline) = {
                    let s = stream.lock();
                    (
                        raw_handle_of(&*s),
                        s.read_timeout()
                            .ok()
                            .flatten()
                            .map(|t| std::time::Instant::now() + t),
                    )
                };
                loop {
                    if !self.wait_ready_close_aware(fd, raw, false, deadline)? {
                        // No poll primitive on this target: one plain blocking
                        // read, the pre-2026-08-12 behaviour.
                        let mut s = stream.lock();
                        return s.read(buf);
                    }
                    let mut s = stream.lock();
                    // Re-ask with a zero timeout under the lock. Between the
                    // poll above and this lock another reader on the same fd
                    // could have taken the bytes; issuing the read anyway would
                    // park inside the syscall, which is the exact thing this
                    // function exists to stop doing.
                    if !poll_socket_readiness_timeout(raw, 0)
                        .map(|(r, _)| r)
                        .unwrap_or(true)
                    {
                        drop(s);
                        continue;
                    }
                    return s.read(buf);
                }
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp read",
            )),
        }
    }

    /// Duplicate the underlying TCP socket for `fd`, returning an independent
    /// owned `TcpStream` handle to the same connection.
    ///
    /// Used by the asynchronous-socket completion path: a worker thread needs to
    /// perform a *blocking* read on the connection while the application keeps
    /// issuing `tcp_write`s (Future-form `AsynchronousSocketChannel.write`) on the
    /// same fd. Holding the per-fd `Mutex<TcpStream>` across a blocking read would
    /// deadlock those writes, so the reader takes a `try_clone()` handle (a second
    /// OS handle onto the same full-duplex socket) and reads on it lock-free while
    /// writes continue through the original entry. Returns an error if `fd` is not
    /// a live TCP stream.
    pub fn try_clone_tcp(&self, fd: FdId) -> Result<std::net::TcpStream, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp clone"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => stream.lock().try_clone(),
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp clone",
            )),
        }
    }

    /// Write to a TCP stream. Returns bytes written.
    ///
    /// # Close-awareness (2026-08-12)
    ///
    /// The write twin of [`tcp_read`](Self::tcp_read). A blocking `send` parks
    /// behind peer backpressure exactly as a `recv` parks behind peer silence,
    /// and measured on this host (`SendWake.java`, Windows 11, JDK 25.0.3, and
    /// recorded in `socket_channel::write_close_aware`) a writer parked in a
    /// blocking-mode `send` is *still* parked 6 s after another thread issues
    /// `shutdown(SHUT_WR)`. Only closing the handle woke it, and this table
    /// cannot close a handle a writer is mid-syscall on.
    ///
    /// Unlike `socket_channel::write_close_aware` this does NOT loop until the
    /// whole payload is out: `tcp_write` reports a short write to its caller,
    /// which is the contract it already had. One write-readiness wait, then one
    /// `send`.
    pub fn tcp_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp write"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => {
                if self.fd_is_nonblocking(fd) {
                    let mut s = stream.lock();
                    return s.write(data);
                }
                let (raw, deadline) = {
                    let s = stream.lock();
                    (
                        raw_handle_of(&*s),
                        s.write_timeout()
                            .ok()
                            .flatten()
                            .map(|t| std::time::Instant::now() + t),
                    )
                };
                let _ = self.wait_ready_close_aware(fd, raw, true, deadline)?;
                let mut s = stream.lock();
                s.write(data)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tcp write",
            )),
        }
    }

    /// Set non-blocking mode on a TCP stream or listener.
    pub fn tcp_set_nonblocking(&self, fd: FdId, nonblocking: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        let applied = match &*entry {
            FileEntry::TcpStream(stream) => stream.lock().set_nonblocking(nonblocking),
            FileEntry::TcpListener { listener, .. } => listener.lock().set_nonblocking(nonblocking),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        };
        // Record only what actually reached the socket, so the side table can
        // never claim a mode the OS refused. See the `nonblocking` field.
        if applied.is_ok() {
            self.record_blocking_mode(fd, nonblocking);
        }
        applied
    }

    /// Get the local address of a TCP stream or listener.
    pub fn tcp_local_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => Ok(stream.lock().local_addr()?.to_string()),
            FileEntry::TcpListener { listener, .. } => {
                Ok(listener.lock().local_addr()?.to_string())
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get the peer address of a TCP stream.
    pub fn tcp_peer_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(stream) => Ok(stream.lock().peer_addr()?.to_string()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Check if a fd is ready for read/write (non-blocking poll).
    /// Returns (readable, writable).
    pub fn poll_ready(&self, fd: FdId) -> (bool, bool) {
        let Some(entry) = self.get_entry(fd) else {
            return (false, false);
        };
        match &*entry {
            FileEntry::UdpSocket(sock) => {
                // Non-destructive readiness probe via the OS `poll`
                // primitive: it neither flips the socket's persistent
                // blocking mode nor consumes the pending datagram, so a
                // concurrent blocking `udp_recv` on the same fd can never
                // observe a transient non-blocking window. The `Arc<FileEntry>`
                // clone taken above keeps the raw handle alive for the call.
                poll_socket_readiness(sock)
            }
            FileEntry::TcpStream(stream) => {
                // Non-destructive readiness probe — see the UdpSocket arm.
                // Avoids the previous set_nonblocking(true)/peek/
                // set_nonblocking(false) dance, which could race a
                // concurrent blocking `tcp_read` into a spurious
                // `WouldBlock` and also clobbered a deliberately
                // non-blocking socket back to blocking.
                let s = stream.lock();
                poll_socket_readiness(&*s)
            }
            FileEntry::TcpListener { listener, pending } => {
                // A connection already stashed by a prior poll counts as
                // readable without touching the OS backlog.
                if !pending.lock().is_empty() {
                    return (true, false);
                }
                // Non-destructive readiness probe: `poll` reports the
                // listening socket readable when a connection is waiting
                // in the backlog, without `accept()`-ing it and without
                // touching the listener's blocking mode.
                let l = listener.lock();
                let (readable, _) = poll_socket_readiness(&*l);
                (readable, false) // listeners are readable (acceptable), not writable
            }
            FileEntry::FileRead(_) => (true, false),
            FileEntry::FileWrite(_) => (false, true),
            FileEntry::FileReadWrite(_) => (true, true),
            FileEntry::PipeRead(buf) => {
                let b = buf.lock();
                (!b.is_empty(), false)
            }
            FileEntry::PipeWrite(buf) => {
                let b = buf.lock();
                (false, b.len() < PIPE_BUFFER_CAPACITY)
            }
            _ => (false, false),
        }
    }

    // -----------------------------------------------------------------------
    // TCP socket options
    // -----------------------------------------------------------------------

    /// Set TCP_NODELAY on a TCP stream.
    pub fn tcp_set_nodelay(&self, fd: FdId, nodelay: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_nodelay(nodelay),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get TCP_NODELAY on a TCP stream.
    pub fn tcp_nodelay(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().nodelay(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set read timeout on a TCP stream.
    pub fn tcp_set_read_timeout(
        &self,
        fd: FdId,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_read_timeout(timeout),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get read timeout on a TCP stream.
    pub fn tcp_read_timeout(&self, fd: FdId) -> Result<Option<Duration>, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().read_timeout(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set write timeout on a TCP stream.
    pub fn tcp_set_write_timeout(
        &self,
        fd: FdId,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_write_timeout(timeout),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set TTL on a TCP stream.
    pub fn tcp_set_ttl(&self, fd: FdId, ttl: u32) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => s.lock().set_ttl(ttl),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_KEEPALIVE on a TCP stream (via socket2).
    pub fn tcp_set_keepalive(&self, fd: FdId, keepalive: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_keepalive(keepalive)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_KEEPALIVE on a TCP stream.
    pub fn tcp_keepalive(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.keepalive()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_LINGER on a TCP stream.
    pub fn tcp_set_linger(&self, fd: FdId, linger: Option<Duration>) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_linger(linger)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_LINGER on a TCP stream.
    pub fn tcp_linger(&self, fd: FdId) -> Result<Option<Duration>, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.linger()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_SNDBUF on a TCP stream.
    pub fn tcp_set_send_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_send_buffer_size(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_SNDBUF on a TCP stream.
    pub fn tcp_send_buffer_size(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.send_buffer_size()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_RCVBUF on a TCP stream.
    pub fn tcp_set_recv_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_recv_buffer_size(size)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Get SO_RCVBUF on a TCP stream.
    pub fn tcp_recv_buffer_size(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.recv_buffer_size()
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Set SO_REUSEADDR on a TCP listener (via socket2).
    pub fn tcp_set_reuse_address(&self, fd: FdId, reuse: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpListener { listener, .. } => {
                let listener = listener.lock();
                let sock = socket2::SockRef::from(&*listener);
                sock.set_reuse_address(reuse)
            }
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let sock = socket2::SockRef::from(&*stream);
                sock.set_reuse_address(reuse)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    /// Estimate available bytes on a TCP stream using peek.
    ///
    /// Mirrors the rewrite documented at `fd_table.rs:1001` for
    /// `poll_ready`: the previous implementation flipped the socket
    /// to non-blocking, called `peek`, then flipped it back. That
    /// pattern (a) races against a concurrent blocking `tcp_read` on
    /// the same fd into a spurious `WouldBlock`, and (b) clobbers the
    /// blocking flag the caller had deliberately set — every
    /// `tcp_available` call would silently undo `set_nonblocking(true)`.
    ///
    /// New strategy: do a non-destructive readiness probe first. If
    /// the kernel reports the socket as not-readable, return 0 without
    /// touching the socket. If it is readable, the kernel already has
    /// buffered data and a `peek` is guaranteed not to block — no
    /// flag-toggling required.
    pub fn tcp_available(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp"))?;
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                let (readable, _) = poll_socket_readiness(&*stream);
                if !readable {
                    // Nothing buffered in the kernel — no bytes
                    // available. Crucially, the socket's blocking flag
                    // is left exactly as the caller configured it.
                    return Ok(0);
                }
                // Readable per poll(); peek without flag toggling. On
                // a blocking stream that the kernel says has data this
                // returns immediately with the buffered byte count.
                let mut buf = [0u8; 8192];
                let avail = match stream.peek(&mut buf) {
                    Ok(n) => n,
                    // POLLIN with WouldBlock means readiness raced
                    // away (a concurrent reader drained the buffer).
                    // That's a snapshot estimate, return 0.
                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => 0,
                    Err(_) => 0,
                };
                Ok(avail)
            }
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for tcp")),
        }
    }

    // -----------------------------------------------------------------------
    // UDP socket options
    // -----------------------------------------------------------------------

    /// Set SO_REUSEADDR on a UDP socket via socket2.
    pub fn udp_set_reuse_address(&self, fd: FdId, on: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).set_reuse_address(on),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set SO_BROADCAST on a UDP socket.
    pub fn udp_set_broadcast(&self, fd: FdId, on: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.set_broadcast(on),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set TTL on a UDP socket.
    pub fn udp_set_ttl(&self, fd: FdId, ttl: u32) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.set_ttl(ttl),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set read timeout on a UDP socket.
    pub fn udp_set_read_timeout(
        &self,
        fd: FdId,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.set_read_timeout(timeout),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set SO_SNDBUF on a UDP socket.
    pub fn udp_set_send_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).set_send_buffer_size(size),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// The raw OS descriptor of a UDP socket, for the options no portable
    /// wrapper exposes.
    ///
    /// `jdk.net.ExtendedSocketOptions`' `IP_DONTFRAGMENT` is a datagram option,
    /// so `native-io`'s extended-option bridge has to reach a socket that lives
    /// in THIS table rather than in either TCP registry. `None` for a handle
    /// that is not a UDP socket.
    #[cfg(unix)]
    pub fn udp_raw_fd(&self, fd: FdId) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        match &*self.get_entry(fd)? {
            FileEntry::UdpSocket(s) => Some(s.as_raw_fd()),
            _ => None,
        }
    }

    /// Windows twin of [`Self::udp_raw_fd`], returning the raw `SOCKET`.
    #[cfg(windows)]
    pub fn udp_raw_socket(&self, fd: FdId) -> Option<u64> {
        use std::os::windows::io::AsRawSocket;
        match &*self.get_entry(fd)? {
            FileEntry::UdpSocket(s) => Some(s.as_raw_socket()),
            _ => None,
        }
    }

    /// The raw OS descriptor of a TCP stream, for the same reason as
    /// [`Self::udp_raw_fd`]: `jdk.net.ExtendedSocketOptions`' keepalive knobs
    /// have no portable wrapper and must reach `setsockopt` directly.
    #[cfg(unix)]
    pub fn tcp_raw_fd(&self, fd: FdId) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        match &*self.get_entry(fd)? {
            FileEntry::TcpStream(s) => Some(s.lock().as_raw_fd()),
            _ => None,
        }
    }

    /// Windows twin of [`Self::tcp_raw_fd`], returning the raw `SOCKET`.
    #[cfg(windows)]
    pub fn tcp_raw_socket(&self, fd: FdId) -> Option<u64> {
        use std::os::windows::io::AsRawSocket;
        match &*self.get_entry(fd)? {
            FileEntry::TcpStream(s) => Some(s.lock().as_raw_socket()),
            _ => None,
        }
    }

    /// Set the IP type-of-service / DSCP byte on a UDP socket (IP_TOS).
    ///
    /// Added so `DatagramChannel.setTrafficClass` could stop being a silent
    /// no-op. `FileEntry` and `get_entry` are private to this module and
    /// `native-io` has no `socket2` dependency, so this could not be written
    /// on the caller's side.
    ///
    /// The option is genuinely advisory — routers may ignore the bits — but
    /// "the network might not honour it" is not the same as "we never asked",
    /// and only the latter was true before.
    pub fn udp_set_tos(&self, fd: FdId, tos: u32) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).set_tos(tos),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Dissolve a UDP socket's association — POSIX `connect(AF_UNSPEC)`.
    ///
    /// Backs `DatagramSocket.disconnect()` / `DatagramChannel.disconnect()`,
    /// which were no-ops that cleared a Java-side `connected` flag while the
    /// kernel kept filtering datagrams to the old peer. That only became a real
    /// gap once `connect` was implemented for real: before, both halves were
    /// vacuous and agreed; after, the socket stayed connected while the object
    /// claimed otherwise.
    ///
    /// Neither `std::net::UdpSocket` nor `socket2::SockRef` exposes this, so it
    /// goes through the raw fd. Both platforms report an error for the
    /// AF_UNSPEC form even when it works (Linux commonly `EAFNOSUPPORT`,
    /// Winsock `WSAEAFNOSUPPORT`) — the disassociation still happens, so that
    /// one code is treated as success rather than surfaced.
    pub fn udp_disconnect(&self, fd: FdId) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        let FileEntry::UdpSocket(s) = &*entry else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"));
        };
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let mut addr: libc::sockaddr = unsafe { std::mem::zeroed() };
            addr.sa_family = libc::AF_UNSPEC as libc::sa_family_t;
            let rc = unsafe {
                libc::connect(
                    s.as_raw_fd(),
                    &addr as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr>() as libc::socklen_t,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAFNOSUPPORT) {
                return Ok(());
            }
            Err(err)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            const WSAEAFNOSUPPORT: i32 = 10047;

            // Declared here rather than at module scope to match the local
            // `#[link(name = "ws2_32")]` blocks already in this file
            // (`disable_udp_connreset`, and the one near line 316).
            #[link(name = "ws2_32")]
            unsafe extern "system" {
                fn connect(s: usize, name: *const u8, namelen: i32) -> i32;
                fn WSAGetLastError() -> i32;
            }

            // `sockaddr` is 16 bytes; all-zero gives sa_family = AF_UNSPEC (0).
            let addr = [0u8; 16];
            let rc = unsafe { connect(s.as_raw_socket() as usize, addr.as_ptr(), 16) };
            if rc == 0 {
                return Ok(());
            }
            let code = unsafe { WSAGetLastError() };
            if code == WSAEAFNOSUPPORT {
                return Ok(());
            }
            Err(io::Error::from_raw_os_error(code))
        }
    }

    /// Read an integer Winsock option from a UDP socket by its Java fd-table id.
    ///
    /// This is intentionally Windows-only: `WindowsSocketOptions` receives
    /// CratonVM handle ids, not raw `SOCKET` values, so native-io needs this
    /// table-owned bridge for DatagramSocket/IP_DONTFRAGMENT.
    #[cfg(windows)]
    pub fn udp_get_socket_option_i32(
        &self,
        fd: FdId,
        level: i32,
        option: i32,
    ) -> Result<i32, io::Error> {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn getsockopt(
                s: usize,
                level: i32,
                optname: i32,
                optval: *mut u8,
                optlen: *mut i32,
            ) -> i32;
            fn WSAGetLastError() -> i32;
        }
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        let FileEntry::UdpSocket(socket) = &*entry else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"));
        };
        let mut value = 0i32;
        let mut size = std::mem::size_of::<i32>() as i32;
        let rc = unsafe {
            getsockopt(
                socket.as_raw_socket() as usize,
                level,
                option,
                (&mut value as *mut i32).cast(),
                &mut size,
            )
        };
        if rc == 0 {
            Ok(value)
        } else {
            Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
        }
    }

    /// Set an integer Winsock option on a UDP socket by its Java fd-table id.
    #[cfg(windows)]
    pub fn udp_set_socket_option_i32(
        &self,
        fd: FdId,
        level: i32,
        option: i32,
        value: i32,
    ) -> Result<(), io::Error> {
        use std::os::windows::io::AsRawSocket;
        #[link(name = "ws2_32")]
        unsafe extern "system" {
            fn setsockopt(
                s: usize,
                level: i32,
                optname: i32,
                optval: *const u8,
                optlen: i32,
            ) -> i32;
            fn WSAGetLastError() -> i32;
        }
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        let FileEntry::UdpSocket(socket) = &*entry else {
            return Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"));
        };
        let rc = unsafe {
            setsockopt(
                socket.as_raw_socket() as usize,
                level,
                option,
                (&value as *const i32).cast(),
                std::mem::size_of::<i32>() as i32,
            )
        };
        if rc == 0 {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(unsafe { WSAGetLastError() }))
        }
    }

    /// Set SO_RCVBUF on a UDP socket.
    pub fn udp_set_recv_buffer_size(&self, fd: FdId, size: usize) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).set_recv_buffer_size(size),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Join a multicast group on a UDP socket (IPv4).
    pub fn udp_join_multicast_v4(
        &self,
        fd: FdId,
        multiaddr: &std::net::Ipv4Addr,
        interface: &std::net::Ipv4Addr,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.join_multicast_v4(multiaddr, interface),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Leave a multicast group on a UDP socket (IPv4).
    pub fn udp_leave_multicast_v4(
        &self,
        fd: FdId,
        multiaddr: &std::net::Ipv4Addr,
        interface: &std::net::Ipv4Addr,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.leave_multicast_v4(multiaddr, interface),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Join a multicast group on a UDP socket (IPv6).
    pub fn udp_join_multicast_v6(
        &self,
        fd: FdId,
        multiaddr: &std::net::Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.join_multicast_v6(multiaddr, interface),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Leave a multicast group on a UDP socket (IPv6).
    pub fn udp_leave_multicast_v6(
        &self,
        fd: FdId,
        multiaddr: &std::net::Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.leave_multicast_v6(multiaddr, interface),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back SO_BROADCAST (the setter is `udp_set_broadcast`).
    pub fn udp_broadcast(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.broadcast(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set the IPv4 multicast TTL (`IP_MULTICAST_TTL`). Distinct from
    /// `udp_set_ttl`, which is the unicast `IP_TTL`.
    pub fn udp_set_multicast_ttl_v4(&self, fd: FdId, ttl: u32) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.set_multicast_ttl_v4(ttl),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back the IPv4 multicast TTL.
    pub fn udp_multicast_ttl_v4(&self, fd: FdId) -> Result<u32, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.multicast_ttl_v4(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    // -----------------------------------------------------------------------
    // UDP option READ-BACK
    //
    // Every setter above landed without its getter, which is only invisible
    // while the option surface is write-only. `DatagramChannel.getOption` is
    // the reader, and without these it would have to answer from a Java-side
    // side store alone — i.e. report what was last *requested* rather than what
    // the socket actually carries, and answer a fabricated default for a
    // channel nothing has set. These ask the socket.
    // -----------------------------------------------------------------------

    /// The peer a UDP socket is connected to, or an error when it is not
    /// connected. Backs `DatagramChannel.remoteAddress()` — the package-private
    /// accessor `sun.nio.ch.DatagramSocketAdaptor.getRemoteSocketAddress()` and
    /// `getPort()` call, and which had no bridge at all.
    pub fn udp_peer_addr(&self, fd: FdId) -> Result<String, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.peer_addr().map(|a| unmap_v4_mapped(a).to_string()),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back SO_REUSEADDR (the setter is `udp_set_reuse_address`).
    pub fn udp_reuse_address(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).reuse_address(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back SO_RCVBUF.
    pub fn udp_recv_buffer_size(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).recv_buffer_size(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back SO_SNDBUF.
    pub fn udp_send_buffer_size(&self, fd: FdId) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).send_buffer_size(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back IP_TOS (the setter is `udp_set_tos`).
    pub fn udp_tos(&self, fd: FdId) -> Result<u32, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).tos(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set IP_MULTICAST_LOOP (IPv4).
    pub fn udp_set_multicast_loop_v4(&self, fd: FdId, on: bool) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.set_multicast_loop_v4(on),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back IP_MULTICAST_LOOP (IPv4).
    pub fn udp_multicast_loop_v4(&self, fd: FdId) -> Result<bool, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => s.multicast_loop_v4(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Set IP_MULTICAST_IF (IPv4) to the interface owning `interface_addr`.
    ///
    /// The JDK's option value is a `NetworkInterface`; the socket-level option
    /// is an address, so the caller resolves the interface to one of its IPv4
    /// addresses first (`native-io`'s `dc_set_option`).
    pub fn udp_set_multicast_if_v4(
        &self,
        fd: FdId,
        interface_addr: &std::net::Ipv4Addr,
    ) -> Result<(), io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).set_multicast_if_v4(interface_addr),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    /// Read back IP_MULTICAST_IF (IPv4) as the configured interface address.
    /// `0.0.0.0` means "no interface selected", which the JDK reports as null.
    pub fn udp_multicast_if_v4(&self, fd: FdId) -> Result<std::net::Ipv4Addr, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for udp"))?;
        match &*entry {
            FileEntry::UdpSocket(s) => socket2::SockRef::from(s).multicast_if_v4(),
            _ => Err(io::Error::new(io::ErrorKind::NotFound, "bad fd for udp")),
        }
    }

    // -----------------------------------------------------------------------
    // Pipe channels (in-memory)
    // -----------------------------------------------------------------------

    /// Create an in-memory pipe. Returns (read_fd, write_fd).
    pub fn open_pipe(&self) -> (FdId, FdId) {
        let buf = Arc::new(Mutex::new(VecDeque::with_capacity(
            PIPE_BUFFER_INITIAL_CAPACITY,
        )));
        let read_fd = self.alloc_fd_or_abort();
        let write_fd = self.alloc_fd_or_abort();
        let mut entries = self.entries.write();
        entries.insert(read_fd, Arc::new(FileEntry::PipeRead(buf.clone())));
        entries.insert(write_fd, Arc::new(FileEntry::PipeWrite(buf)));
        (read_fd, write_fd)
    }

    /// Read from the read end of a pipe.
    ///
    /// Returns the number of bytes read. An empty buffer is NOT unconditionally
    /// reported as EOF: `Ok(0)` (true EOF) is only returned once the write end
    /// has been closed. While a writer is still open and the buffer is empty,
    /// this returns `ErrorKind::WouldBlock` so a `FileInputStream` reader does
    /// not mistake "no data yet" for end-of-stream.
    ///
    /// The pipe buffer is shared via `Arc`: `PipeRead` holds one clone and each
    /// open `PipeWrite` holds another. So a `strong_count` of exactly 1 means
    /// only this read end remains — every writer has been dropped/closed.
    pub fn pipe_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pipe read"))?;
        match &*entry {
            FileEntry::PipeRead(pipe) => {
                let mut p = pipe.lock();
                let n = buf.len().min(p.len());
                if n == 0 {
                    // Buffer empty: distinguish a still-open writer from true EOF.
                    let writer_open = Arc::strong_count(pipe) > 1;
                    drop(p);
                    if writer_open {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "pipe empty, writer still open",
                        ));
                    }
                    return Ok(0);
                }
                for (i, b) in p.drain(..n).enumerate() {
                    buf[i] = b;
                }
                Ok(n)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for pipe read",
            )),
        }
    }

    /// Write to the write end of a pipe. Returns bytes written.
    pub fn pipe_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for pipe write"))?;
        match &*entry {
            FileEntry::PipeWrite(pipe) => {
                if data.is_empty() {
                    return Ok(0);
                }
                let mut p = pipe.lock();
                let available = PIPE_BUFFER_CAPACITY.saturating_sub(p.len());
                if available == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WouldBlock,
                        "pipe buffer full",
                    ));
                }
                let n = data.len().min(available);
                p.extend(&data[..n]);
                Ok(n)
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for pipe write",
            )),
        }
    }

    // =========================================================================
    // TLS operations
    // =========================================================================

    /// Connect via TLS to the given host:port. Returns the fd_id.
    ///
    /// # Security
    ///
    /// `host`/`port` are used **verbatim** to connect with no sandbox/SSRF
    /// check performed here. When these originate from Java-controlled input,
    /// the caller (the VM / native-io sandbox layer) MUST sanitize them
    /// before calling — this function deliberately does not enforce that, to
    /// avoid duplicating (and potentially conflicting with) the VM's own
    /// sandbox policy.
    pub fn open_tls_connect(&self, host: &str, port: u16) -> Result<FdId, io::Error> {
        // fds only need to be unique, not contiguous — on overflow we
        // simply fail without rolling the counter back (a `fetch_sub`
        // rollback would be racy and pointless).
        let fd = self.next_fd.fetch_add(1, Ordering::Relaxed);
        if fd >= u32::MAX - 16 {
            return Err(io::Error::other("file descriptor limit exceeded"));
        }
        let addr = format!("{}:{}", host, port);
        let tcp = std::net::TcpStream::connect(&addr)?;
        let connector = native_tls::TlsConnector::new()
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        let tls_stream = connector
            .connect(host, tcp)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
        self.entries
            .write()
            .insert(fd, Arc::new(FileEntry::TlsStream(Mutex::new(tls_stream))));
        Ok(fd)
    }

    /// Read from a TLS stream.
    pub fn tls_read(&self, fd: FdId, buf: &mut [u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tls read"))?;
        match &*entry {
            FileEntry::TlsStream(stream) => {
                let mut s = stream.lock();
                s.read(buf)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tls read",
            )),
        }
    }

    /// Write to a TLS stream.
    pub fn tls_write(&self, fd: FdId, data: &[u8]) -> Result<usize, io::Error> {
        let entry = self
            .get_entry(fd)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "bad fd for tls write"))?;
        match &*entry {
            FileEntry::TlsStream(stream) => {
                let mut s = stream.lock();
                s.write(data)
                    .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
            }
            _ => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "bad fd for tls write",
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Capability-checked openers
// ---------------------------------------------------------------------------

/// Why a capability-checked open failed.
///
/// The two causes are genuinely different and callers translate them
/// differently: a [`Denied`](Self::Denied) must surface to Java as a
/// `SecurityException` (the operation was refused *before* the syscall and
/// must not be retried), an [`Io`](Self::Io) as the `IOException` the JDK
/// method already documents.
#[derive(Debug)]
pub enum FdCapabilityError {
    /// The VM's capability policy refused the operation. **No syscall was
    /// issued**, no fd was allocated, and nothing on the filesystem or network
    /// was touched.
    Denied(crate::capability::CapabilityDenied),
    /// The operation was permitted and the OS refused it.
    Io(io::Error),
}

impl std::fmt::Display for FdCapabilityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FdCapabilityError::Denied(d) => write!(f, "{d}"),
            FdCapabilityError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for FdCapabilityError {}

impl From<crate::capability::CapabilityDenied> for FdCapabilityError {
    fn from(denied: crate::capability::CapabilityDenied) -> Self {
        FdCapabilityError::Denied(denied)
    }
}

impl From<io::Error> for FdCapabilityError {
    fn from(error: io::Error) -> Self {
        FdCapabilityError::Io(error)
    }
}

impl From<FdCapabilityError> for io::Error {
    /// Lossy fallback for the many call sites that can only produce an
    /// `io::Error` today. A denial becomes `PermissionDenied`, which is what
    /// those sites already map to the JDK's `AccessDeniedException` /
    /// `FileSystemException`. Prefer matching on [`FdCapabilityError`]
    /// directly so the denial can surface as a `SecurityException`.
    fn from(error: FdCapabilityError) -> io::Error {
        match error {
            FdCapabilityError::Io(e) => e,
            FdCapabilityError::Denied(d) => {
                io::Error::new(io::ErrorKind::PermissionDenied, d.to_string())
            }
        }
    }
}

impl From<FdCapabilityError> for cratonvm_types::error::MethodCallFailed {
    fn from(error: FdCapabilityError) -> cratonvm_types::error::MethodCallFailed {
        match error {
            FdCapabilityError::Denied(d) => d.into(),
            FdCapabilityError::Io(e) => cratonvm_types::error::RuntimeError::IOException {
                message: e.to_string(),
            }
            .into(),
        }
    }
}

/// Capability-checked wrappers around the raw openers.
///
/// # Why these are separate methods rather than a check inside `open_read`
///
/// [`FileDescriptorTable`] is `&self`-only and owns no VM identity — it cannot
/// find a policy on its own, and giving it one would put a process-global
/// lookup on the I/O path, which is the defect being fixed. The policy is
/// therefore supplied by the caller, which is the native that knows which VM
/// it is running for. Everything on the old, uncheck path keeps working
/// unchanged; migrating a call site is a one-token edit plus threading
/// `&CapabilitySet` in.
///
/// # Order of operations
///
/// The check happens **before the fd is reserved and before the syscall**, so
/// a denial has no observable effect: no fd is consumed, no file is created,
/// no connection is attempted.
impl FileDescriptorTable {
    /// [`open_read`](Self::open_read), gated on
    /// [`Capability::FileRead`](crate::capability::Capability::FileRead).
    #[track_caller]
    pub fn open_read_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        path: &str,
    ) -> Result<FdId, FdCapabilityError> {
        caps.check(crate::capability::Capability::file_read(path))?;
        Ok(self.open_read(path)?)
    }

    /// [`open_write`](Self::open_write), gated on
    /// [`Capability::FileWrite`](crate::capability::Capability::FileWrite).
    #[track_caller]
    pub fn open_write_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        path: &str,
        append: bool,
    ) -> Result<FdId, FdCapabilityError> {
        caps.check(crate::capability::Capability::file_write(path))?;
        Ok(self.open_write(path, append)?)
    }

    /// [`open_read_write`](Self::open_read_write), gated on **both**
    /// `FileRead` and `FileWrite` — the fd it returns can do either, so one
    /// grant is not enough.
    #[track_caller]
    pub fn open_read_write_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        path: &str,
        create: bool,
    ) -> Result<FdId, FdCapabilityError> {
        caps.check(crate::capability::Capability::file_read(path))?;
        caps.check(crate::capability::Capability::file_write(path))?;
        Ok(self.open_read_write(path, create)?)
    }

    /// [`open_random_access`](Self::open_random_access), gated on `FileRead`
    /// and — when `write` is set — additionally on `FileWrite`.
    #[track_caller]
    pub fn open_random_access_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        path: &str,
        write: bool,
    ) -> Result<FdId, FdCapabilityError> {
        caps.check(crate::capability::Capability::file_read(path))?;
        if write {
            caps.check(crate::capability::Capability::file_write(path))?;
        }
        Ok(self.open_random_access(path, write)?)
    }

    /// [`open_tcp_connect`](Self::open_tcp_connect), gated on
    /// [`Capability::Network`](crate::capability::Capability::Network) for the
    /// destination endpoint.
    #[track_caller]
    pub fn open_tcp_connect_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        addr: &str,
    ) -> Result<FdId, FdCapabilityError> {
        caps.check(crate::capability::Capability::network(addr))?;
        Ok(self.open_tcp_connect(addr)?)
    }

    /// [`open_tcp_listener`](Self::open_tcp_listener), gated on `Network` for
    /// the *bind* endpoint. Binding is a capability in its own right: a listener
    /// on `0.0.0.0:8080` exposes the host, it does not merely reach out from it.
    #[track_caller]
    pub fn open_tcp_listener_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        addr: &str,
    ) -> Result<FdId, FdCapabilityError> {
        caps.check(crate::capability::Capability::network(addr))?;
        Ok(self.open_tcp_listener(addr)?)
    }

    /// [`open_udp`](Self::open_udp), gated on `Network`. A `None` bind address
    /// is checked as [`Scope::Any`](crate::capability::Scope::Any) — the call
    /// site cannot name an endpoint, so only an unscoped grant admits it.
    #[track_caller]
    pub fn open_udp_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        bind_addr: Option<&str>,
    ) -> Result<FdId, FdCapabilityError> {
        let scope = match bind_addr {
            Some(addr) => crate::capability::Scope::endpoint_str(addr),
            None => crate::capability::Scope::Any,
        };
        caps.check(crate::capability::Capability::Network(scope))?;
        Ok(self.open_udp(bind_addr)?)
    }

    /// [`open_udp_dual_stack`](Self::open_udp_dual_stack) behind the same
    /// network capability gate as [`open_udp_checked`](Self::open_udp_checked).
    ///
    /// The scope is the wildcard endpoint the socket will actually hold, so a
    /// policy that would refuse `open_udp(Some("0.0.0.0:0"))` refuses this too.
    pub fn open_udp_dual_stack_checked(
        &self,
        caps: &crate::capability::CapabilitySet,
        port: u16,
    ) -> Result<FdId, FdCapabilityError> {
        let scope = crate::capability::Scope::endpoint_str(&format!("0.0.0.0:{port}"));
        caps.check(crate::capability::Capability::Network(scope))?;
        Ok(self.open_udp_dual_stack_port(port)?)
    }
}

impl Default for FileDescriptorTable {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for FileDescriptorTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileDescriptorTable")
            .field("open_fds", &self.entries.read().len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering as AtomOrd};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Helper: create a temp file with the given content and return its path.
    fn temp_file_with(content: &str) -> String {
        let id = TEST_COUNTER.fetch_add(1, AtomOrd::Relaxed);
        let dir = std::env::temp_dir();
        let name = format!("cratonvm_fdtest_{}_{}.txt", std::process::id(), id);
        let path = dir.join(name);
        let mut f = fs::File::create(&path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        drop(f);
        path.to_string_lossy().into_owned()
    }

    /// Helper: create a unique temp file path (file may or may not exist).
    fn temp_path(suffix: &str) -> String {
        let id = TEST_COUNTER.fetch_add(1, AtomOrd::Relaxed);
        let dir = std::env::temp_dir();
        let name = format!(
            "cratonvm_fdtest_{}_{}_{}.txt",
            std::process::id(),
            id,
            suffix
        );
        dir.join(name).to_string_lossy().into_owned()
    }

    // -----------------------------------------------------------------------
    // Construction
    // -----------------------------------------------------------------------

    #[test]
    fn new_table_has_three_standard_fds() {
        let table = FileDescriptorTable::new();
        // stdin(0), stdout(1), stderr(2) are pre-registered
        assert_eq!(table.entries.read().len(), 3);
    }

    #[test]
    fn default_is_same_as_new() {
        let table = FileDescriptorTable::default();
        assert_eq!(table.entries.read().len(), 3);
    }

    #[test]
    fn debug_format_shows_open_fds() {
        let table = FileDescriptorTable::new();
        let dbg = format!("{:?}", table);
        assert!(dbg.contains("open_fds: 3"));
    }

    // -----------------------------------------------------------------------
    // open_read
    // -----------------------------------------------------------------------

    #[test]
    fn open_read_returns_fd_ge_3() {
        let path = temp_file_with("hello");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert!(fd >= 3);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_read_nonexistent_file_errors() {
        let table = FileDescriptorTable::new();
        let result = table.open_read("/tmp/cratonvm_fdtest_nonexistent_xyzzy.txt");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // open_write
    // -----------------------------------------------------------------------

    #[test]
    fn open_write_creates_file() {
        let path = temp_path("write_create");
        let _ = fs::remove_file(&path);
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        assert!(fd >= 3);
        assert!(fs::metadata(&path).is_ok());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_write_truncate_mode() {
        let path = temp_file_with("old content");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        table.write_string(fd, "new").unwrap();
        table.flush(fd).unwrap();
        table.close(fd).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "new");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_write_append_mode() {
        let path = temp_file_with("first");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, true).unwrap();
        table.write_string(fd, "second").unwrap();
        table.flush(fd).unwrap();
        table.close(fd).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, "firstsecond");
        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // read_line
    // -----------------------------------------------------------------------

    #[test]
    fn read_line_returns_lines_without_newline() {
        let path = temp_file_with("line1\nline2\nline3\n");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some("line1".to_string()));
        assert_eq!(table.read_line(fd).unwrap(), Some("line2".to_string()));
        assert_eq!(table.read_line(fd).unwrap(), Some("line3".to_string()));
        assert_eq!(table.read_line(fd).unwrap(), None); // EOF
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_line_strips_crlf() {
        let path = temp_file_with("hello\r\nworld\r\n");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some("hello".to_string()));
        assert_eq!(table.read_line(fd).unwrap(), Some("world".to_string()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_line_bad_fd_errors() {
        let table = FileDescriptorTable::new();
        let result = table.read_line(999);
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // read_byte / read_bytes
    // -----------------------------------------------------------------------

    #[test]
    fn read_byte_returns_bytes_then_eof() {
        let path = temp_file_with("AB");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_byte(fd).unwrap(), b'A' as i32);
        assert_eq!(table.read_byte(fd).unwrap(), b'B' as i32);
        assert_eq!(table.read_byte(fd).unwrap(), -1); // EOF
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_bytes_fills_buffer() {
        let path = temp_file_with("hello world");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        let mut buf = [0u8; 5];
        let n = table.read_bytes(fd, &mut buf).unwrap();
        assert_eq!(n, 5);
        assert_eq!(&buf, b"hello");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_byte_bad_fd_errors() {
        let table = FileDescriptorTable::new();
        assert!(table.read_byte(999).is_err());
    }

    #[test]
    fn read_bytes_bad_fd_errors() {
        let table = FileDescriptorTable::new();
        let mut buf = [0u8; 4];
        assert!(table.read_bytes(999, &mut buf).is_err());
    }

    // -----------------------------------------------------------------------
    // write_byte / write_bytes / write_string
    // -----------------------------------------------------------------------

    #[test]
    fn write_byte_to_file() {
        let path = temp_path("write_byte");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        table.write_byte(fd, b'X').unwrap();
        table.flush(fd).unwrap();
        table.close(fd).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "X");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_bytes_to_file() {
        let path = temp_path("write_bytes");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        table.write_bytes(fd, b"hello").unwrap();
        table.flush(fd).unwrap();
        table.close(fd).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_string_to_file() {
        let path = temp_path("write_string");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        table.write_string(fd, "rust jvm").unwrap();
        table.flush(fd).unwrap();
        table.close(fd).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "rust jvm");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_byte_bad_fd_errors() {
        let table = FileDescriptorTable::new();
        assert!(table.write_byte(999, b'X').is_err());
    }

    #[test]
    fn write_bytes_bad_fd_errors() {
        let table = FileDescriptorTable::new();
        assert!(table.write_bytes(999, b"data").is_err());
    }

    // -----------------------------------------------------------------------
    // close
    // -----------------------------------------------------------------------

    #[test]
    fn close_removes_fd() {
        let path = temp_file_with("data");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        table.close(fd).unwrap();
        // After close, reading should fail
        assert!(table.read_byte(fd).is_err());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn close_already_closed_fd_is_noop() {
        let path = temp_file_with("data");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        table.close(fd).unwrap();
        // Second close should not panic
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn close_does_not_remove_stdin_stdout_stderr() {
        let table = FileDescriptorTable::new();
        table.close(0).unwrap();
        table.close(1).unwrap();
        table.close(2).unwrap();
        // Standard fds should still be present
        assert_eq!(table.entries.read().len(), 3);
    }

    // -----------------------------------------------------------------------
    // flush
    // -----------------------------------------------------------------------

    #[test]
    fn flush_nonwritable_fd_is_noop() {
        let path = temp_file_with("data");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        // flush on a read fd should be Ok (no-op for non-writable)
        assert!(table.flush(fd).is_ok());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn flush_nonexistent_fd_is_noop() {
        let table = FileDescriptorTable::new();
        assert!(table.flush(999).is_ok());
    }

    // -----------------------------------------------------------------------
    // available
    // -----------------------------------------------------------------------

    #[test]
    fn available_on_read_fd() {
        let path = temp_file_with("hello");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        // Before any read, buffer may be empty (0) — that's valid
        let avail = table.available(fd).unwrap();
        let _ = avail; // just checking available() didn't error
                       // After a read, there may be more in the buffer
        let _ = table.read_byte(fd);
        let _ = table.available(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn available_bad_fd_errors() {
        let table = FileDescriptorTable::new();
        assert!(table.available(999).is_err());
    }

    #[test]
    fn available_stdin_returns_zero() {
        let table = FileDescriptorTable::new();
        assert_eq!(table.available(0).unwrap(), 0);
    }

    #[test]
    fn pipe_write_caps_buffer_and_reports_backpressure() {
        let table = FileDescriptorTable::new();
        let (read_fd, write_fd) = table.open_pipe();

        let almost_full = vec![0x41; PIPE_BUFFER_CAPACITY - 1];
        assert_eq!(
            table.pipe_write(write_fd, &almost_full).unwrap(),
            PIPE_BUFFER_CAPACITY - 1
        );
        assert_eq!(table.poll_ready(write_fd), (false, true));

        assert_eq!(table.pipe_write(write_fd, b"xy").unwrap(), 1);
        assert_eq!(table.poll_ready(write_fd), (false, false));

        let err = table.pipe_write(write_fd, b"z").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(table.pipe_write(write_fd, b"").unwrap(), 0);

        let mut one = [0u8; 1];
        assert_eq!(table.pipe_read(read_fd, &mut one).unwrap(), 1);
        assert_eq!(table.poll_ready(write_fd), (false, true));
        assert_eq!(table.pipe_write(write_fd, b"z").unwrap(), 1);
    }

    // -----------------------------------------------------------------------
    // Multiple concurrent opens
    // -----------------------------------------------------------------------

    #[test]
    fn multiple_concurrent_opens_get_unique_fds() {
        let p1 = temp_file_with("a");
        let p2 = temp_file_with("b");
        let p3 = temp_file_with("c");
        let table = FileDescriptorTable::new();
        let fd1 = table.open_read(&p1).unwrap();
        let fd2 = table.open_read(&p2).unwrap();
        let fd3 = table.open_read(&p3).unwrap();
        assert_ne!(fd1, fd2);
        assert_ne!(fd2, fd3);
        assert_ne!(fd1, fd3);
        assert!(fd1 >= 3 && fd2 >= 3 && fd3 >= 3);
        let _ = fs::remove_file(&p1);
        let _ = fs::remove_file(&p2);
        let _ = fs::remove_file(&p3);
    }

    // -----------------------------------------------------------------------
    // FD counter overflow protection
    // -----------------------------------------------------------------------

    #[test]
    fn fd_overflow_protection_read() {
        let path = temp_file_with("overflow test");
        let table = FileDescriptorTable::new();
        // Force the counter near the limit
        table.next_fd.store(u32::MAX - 10, Ordering::Relaxed);
        let result = table.open_read(&path);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(err.to_string().contains("file descriptor limit exceeded"));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn fd_overflow_protection_write() {
        let path = temp_path("overflow_write");
        let table = FileDescriptorTable::new();
        table.next_fd.store(u32::MAX - 10, Ordering::Relaxed);
        let result = table.open_write(&path, false);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("file descriptor limit exceeded"));
        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // Round-trip: write then read
    // -----------------------------------------------------------------------

    #[test]
    fn write_then_read_roundtrip() {
        let path = temp_path("roundtrip");
        let table = FileDescriptorTable::new();

        // Write
        let wfd = table.open_write(&path, false).unwrap();
        table.write_string(wfd, "line1\nline2\n").unwrap();
        table.flush(wfd).unwrap();
        table.close(wfd).unwrap();

        // Read back
        let rfd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(rfd).unwrap(), Some("line1".to_string()));
        assert_eq!(table.read_line(rfd).unwrap(), Some("line2".to_string()));
        assert_eq!(table.read_line(rfd).unwrap(), None);
        table.close(rfd).unwrap();
        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // Write to stdout/stderr fds (smoke test — just ensure no panic/error)
    // -----------------------------------------------------------------------

    #[test]
    fn write_to_stdout_fd() {
        let table = FileDescriptorTable::new();
        // Writing to stdout fd should succeed (output goes to test harness)
        assert!(table.write_byte(1, b'\n').is_ok());
        assert!(table.write_bytes(1, b"").is_ok());
        assert!(table.write_string(1, "").is_ok());
    }

    #[test]
    fn write_to_stderr_fd() {
        let table = FileDescriptorTable::new();
        assert!(table.write_byte(2, b'\n').is_ok());
    }

    #[test]
    fn flush_stdout_stderr() {
        let table = FileDescriptorTable::new();
        assert!(table.flush(1).is_ok());
        assert!(table.flush(2).is_ok());
    }

    // -----------------------------------------------------------------------
    // Read from write fd should fail
    // -----------------------------------------------------------------------

    #[test]
    fn read_from_write_fd_errors() {
        let path = temp_path("read_write_fd");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        assert!(table.read_byte(fd).is_err());
        assert!(table.read_line(fd).is_err());
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn write_to_read_fd_errors() {
        let path = temp_file_with("data");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert!(table.write_byte(fd, b'X').is_err());
        assert!(table.write_bytes(fd, b"X").is_err());
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // T2.4.14 — BufferedReader.readLine: must recognize \n, \r\n, and
    // bare \r as line terminators (BufferedReader javadoc contract).
    // -----------------------------------------------------------------------

    #[test]
    fn t2_read_line_unix_lf() {
        let path = temp_file_with("alpha\nbeta\n");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some("alpha".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("beta".into()));
        assert_eq!(table.read_line(fd).unwrap(), None);
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn t2_read_line_windows_crlf_stays_atomic() {
        let path = temp_file_with("one\r\ntwo\r\nthree");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some("one".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("two".into()));
        // Final line without terminator still produces a value.
        assert_eq!(table.read_line(fd).unwrap(), Some("three".into()));
        assert_eq!(table.read_line(fd).unwrap(), None);
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn t2_read_line_classic_mac_cr_only() {
        // Classic Mac OS used a bare \r as the line terminator. The
        // previous implementation returned the entire file as a single
        // line because `BufRead::read_line` only stops on \n.
        let path = temp_file_with("uno\rdos\rtres");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some("uno".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("dos".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("tres".into()));
        assert_eq!(table.read_line(fd).unwrap(), None);
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn t2_read_line_mixed_terminators() {
        // Verifies interleaved \n / \r / \r\n all work in the same file.
        let path = temp_file_with("a\nb\rc\r\nd");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some("a".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("b".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("c".into()));
        assert_eq!(table.read_line(fd).unwrap(), Some("d".into()));
        assert_eq!(table.read_line(fd).unwrap(), None);
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn t2_read_line_empty_line_returns_empty_string() {
        // Consecutive newlines should each yield an empty line, not get
        // collapsed. Same invariant as BufferedReader.readLine.
        let path = temp_file_with("\n\n");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();
        assert_eq!(table.read_line(fd).unwrap(), Some(String::new()));
        assert_eq!(table.read_line(fd).unwrap(), Some(String::new()));
        assert_eq!(table.read_line(fd).unwrap(), None);
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    // -----------------------------------------------------------------------
    // tcp_available — the previous implementation toggled the socket's
    // non-blocking flag (set true / peek / set false), which silently
    // clobbered a non-blocking flag the caller had deliberately set.
    // See the comment at `fd_table.rs:1001` for the analogous rewrite of
    // `poll_ready` that this fix mirrors.
    // -----------------------------------------------------------------------

    /// Helper: spin up a localhost loopback TcpStream pair and return
    /// the client side (so the test owns one end and can probe it).
    /// The server-side accepted socket is kept alive in the returned
    /// guard so the connection doesn't reset under the test.
    fn loopback_pair() -> (
        std::net::TcpStream,
        std::net::TcpListener,
        std::net::TcpStream,
    ) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, listener, server)
    }

    /// The wildcard TCP listener has to accept BOTH loopback families, and it
    /// has to say so in `local_addr()`.
    ///
    /// The regression this pins is one line of behaviour, not one line of code:
    /// `TcpListener::bind("0.0.0.0:0")` is an AF_INET listener, and a `::1`
    /// client of one gets `Connection refused`. That is what all 48
    /// parameterisations of `SSLEngineTest.testMutualAuthDiffCerts` hit, in four
    /// netty SSL classes, once `NetUtil.LOCALHOST` resolved to the IPv6
    /// loopback.
    ///
    /// Skipped rather than failed on a host with no IPv6 stack: the helper's
    /// documented fallback is a v4 wildcard, and asserting the v6 half there
    /// would be asserting the host's configuration.
    #[test]
    fn dual_stack_wildcard_listener_accepts_both_loopback_families() {
        let listener = match open_tcp_dual_stack_listener(0, 0) {
            Ok(l) => l,
            Err(_) => return,
        };
        let local = listener.local_addr().expect("local_addr");
        if local.is_ipv4() {
            // No IPv6 on this host — the documented fallback. Nothing to assert.
            return;
        }
        assert!(local.ip().is_unspecified(), "expected the wildcard, got {local}");
        let port = local.port();

        for host in ["127.0.0.1", "::1"] {
            let target: std::net::SocketAddr = if host.contains(':') {
                format!("[{host}]:{port}").parse().unwrap()
            } else {
                format!("{host}:{port}").parse().unwrap()
            };
            let client = std::net::TcpStream::connect(target)
                .unwrap_or_else(|e| panic!("dual-stack listener refused {target}: {e}"));
            let (server, _) = listener.accept().expect("accept");
            drop(server);
            drop(client);
        }
    }

    #[test]
    fn poll_ready_tcp_reports_os_read_write_bits() {
        let (client, _listener, _server) = loopback_pair();
        let table = FileDescriptorTable::new();
        let fd = table.insert_tcp_stream(client);

        let expected = {
            let entry = table.get_entry(fd).unwrap();
            match &*entry {
                FileEntry::TcpStream(stream) => {
                    let stream = stream.lock();
                    poll_socket_readiness(&*stream)
                }
                _ => unreachable!(),
            }
        };
        assert_eq!(table.poll_ready(fd), expected);
    }

    #[test]
    fn poll_ready_udp_reports_os_read_write_bits() {
        let table = FileDescriptorTable::new();
        let fd = table.open_udp(Some("127.0.0.1:0")).unwrap();

        let expected = {
            let entry = table.get_entry(fd).unwrap();
            match &*entry {
                FileEntry::UdpSocket(sock) => poll_socket_readiness(sock),
                _ => unreachable!(),
            }
        };
        assert_eq!(table.poll_ready(fd), expected);
    }

    /// Differentiates a non-blocking socket from a blocking one by
    /// timing: read on an empty buffer returns `WouldBlock` on *both*
    /// paths (set_read_timeout also surfaces a timeout as WouldBlock
    /// on Linux), so we time how long the read took. A truly non-
    /// blocking read returns essentially immediately; a blocking read
    /// gated by a 200ms timeout returns after ~200ms.
    fn is_still_nonblocking_via_timing(stream: &std::net::TcpStream) -> bool {
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let mut buf = [0u8; 16];
        let start = std::time::Instant::now();
        // We expect this to fail (empty buffer); we only care about
        // *how long* it took to fail. `impl Read for &TcpStream`
        // lets us read through a shared reference.
        let mut reader: &std::net::TcpStream = stream;
        let _ = reader.read(&mut buf);
        let elapsed = start.elapsed();
        // Clear the timeout so we don't perturb later reads in the
        // same test.
        stream.set_read_timeout(None).unwrap();
        // Non-blocking: well under 50ms. Blocking-with-timeout: ~200ms.
        // 100ms is a comfortable midpoint.
        elapsed < Duration::from_millis(100)
    }

    #[test]
    fn tcp_available_preserves_nonblocking_flag_no_data() {
        // Regression: pre-fix `tcp_available` did
        //   set_nonblocking(true); peek(); set_nonblocking(false);
        // which silently clobbered an intentionally non-blocking
        // socket. The new code uses `poll_socket_readiness` first and
        // peeks only when the kernel says there's data — leaving the
        // blocking flag untouched. See `fd_table.rs:1001` for the
        // analogous fix that `poll_ready` already received.
        let (client, _listener, _server) = loopback_pair();
        let table = FileDescriptorTable::new();
        let fd = table.insert_tcp_stream(client);

        // Caller persistently sets non-blocking.
        table.tcp_set_nonblocking(fd, true).unwrap();

        // Probe — there is no data on the wire so this returns Ok(0).
        let avail = table.tcp_available(fd).unwrap();
        assert_eq!(avail, 0, "no data was sent, available should be 0");

        // The persistent non-blocking flag must still be in effect.
        let entry = table.get_entry(fd).unwrap();
        match &*entry {
            FileEntry::TcpStream(s) => {
                let stream = s.lock();
                assert!(
                    is_still_nonblocking_via_timing(&*stream),
                    "tcp_available clobbered the persistent non-blocking flag"
                );
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn tcp_available_preserves_nonblocking_flag_with_data() {
        // Same regression check but with bytes already buffered: the
        // poll path reports "readable" and we hit the peek branch. The
        // non-blocking flag must still survive intact.
        let (client, _listener, mut server) = loopback_pair();
        // Push some bytes from the server end so the client's recv
        // buffer has data ready.
        server.write_all(b"hello world").unwrap();
        server.flush().unwrap();

        let table = FileDescriptorTable::new();
        let fd = table.insert_tcp_stream(client);
        table.tcp_set_nonblocking(fd, true).unwrap();

        // Give the kernel a brief moment to deliver the bytes to the
        // client's recv buffer so `poll` reports readable.
        let mut avail = 0;
        for _ in 0..50 {
            avail = table.tcp_available(fd).unwrap();
            if avail > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(avail > 0, "expected buffered bytes, got {avail}");

        // Persistent non-blocking flag must still be set. Drain the
        // buffered bytes first so the timing check sees an empty
        // recv buffer.
        let entry = table.get_entry(fd).unwrap();
        match &*entry {
            FileEntry::TcpStream(s) => {
                let mut stream = s.lock();
                let mut buf = [0u8; 64];
                let _ = stream.read(&mut buf).unwrap();
                assert!(
                    is_still_nonblocking_via_timing(&*stream),
                    "tcp_available clobbered the persistent non-blocking flag \
                     after the data-present (peek) path"
                );
            }
            _ => unreachable!(),
        }
    }

    // -----------------------------------------------------------------------
    // Capability-checked openers
    // -----------------------------------------------------------------------

    use crate::capability::{Capability, CapabilityMode, CapabilitySet, Scope, VmId};

    fn caps(mode: CapabilityMode, grants: &str) -> CapabilitySet {
        let mut set = CapabilitySet::new(VmId::from_raw(0xFD_0001), mode);
        assert!(
            set.grant_from_list(grants).is_empty(),
            "test grant list must parse"
        );
        set
    }

    #[test]
    fn permissive_checked_openers_behave_exactly_like_the_raw_ones() {
        let path = temp_file_with("hello");
        let table = FileDescriptorTable::new();
        let policy = caps(CapabilityMode::Permissive, "");
        let fd = table
            .open_read_checked(&policy, &path)
            .expect("permissive allows everything");
        let mut buf = [0u8; 8];
        let n = table.read_bytes(fd, &mut buf).unwrap();
        assert_eq!(&buf[..n], b"hello");
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn enforce_denies_an_ungranted_read_without_touching_the_filesystem() {
        let path = temp_file_with("secret");
        let table = FileDescriptorTable::new();
        let policy = caps(CapabilityMode::Enforce, "file-read:/definitely/elsewhere");

        let err = table
            .open_read_checked(&policy, &path)
            .expect_err("path is outside the grant");
        match err {
            FdCapabilityError::Denied(d) => {
                assert_eq!(d.capability, crate::capability::CapabilityKind::FileRead);
                assert_eq!(d.vm, VmId::from_raw(0xFD_0001));
            }
            FdCapabilityError::Io(e) => panic!("expected a denial, got io error {e}"),
        }
        // No fd was consumed by the refused open: the next successful open
        // gets the very next descriptor after the reserved 0/1/2.
        let permissive = caps(CapabilityMode::Permissive, "");
        let fd = table.open_read_checked(&permissive, &path).unwrap();
        assert_eq!(fd, 3, "a denied open must not reserve a descriptor");
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn enforce_denies_a_write_that_would_have_created_the_file() {
        let path = temp_path("cap_denied_create");
        assert!(!std::path::Path::new(&path).exists());
        let table = FileDescriptorTable::new();
        let policy = caps(CapabilityMode::Enforce, "file-read:*");

        assert!(matches!(
            table.open_write_checked(&policy, &path, false),
            Err(FdCapabilityError::Denied(_))
        ));
        assert!(
            !std::path::Path::new(&path).exists(),
            "a denied write must not create the file — the check runs before the syscall"
        );
    }

    #[test]
    fn a_granted_path_prefix_admits_files_under_it() {
        let path = temp_file_with("granted");
        let dir = std::env::temp_dir();
        let table = FileDescriptorTable::new();
        let mut policy = CapabilitySet::new(VmId::from_raw(0xFD_0002), CapabilityMode::Enforce);
        policy.grant(Capability::FileRead(Scope::path(&dir.to_string_lossy())));

        let fd = table
            .open_read_checked(&policy, &path)
            .expect("the temp dir prefix must admit a file inside it");
        table.close(fd).unwrap();

        // ...and a traversal out of it is refused even though it is spelled
        // as a child of the granted prefix.
        let escape = format!("{}/../etc/passwd", dir.to_string_lossy());
        assert!(matches!(
            table.open_read_checked(&policy, &escape),
            Err(FdCapabilityError::Denied(_))
        ));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_write_open_requires_both_capabilities() {
        let path = temp_file_with("rw");
        let table = FileDescriptorTable::new();
        // Read-only grant: an fd that can also write must be refused.
        let read_only = caps(CapabilityMode::Enforce, "file-read:*");
        assert!(matches!(
            table.open_read_write_checked(&read_only, &path, false),
            Err(FdCapabilityError::Denied(_))
        ));
        let both = caps(CapabilityMode::Enforce, "file-read:*;file-write:*");
        let fd = table
            .open_read_write_checked(&both, &path, false)
            .expect("both granted");
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn network_openers_are_gated_on_the_endpoint() {
        let table = FileDescriptorTable::new();
        let policy = caps(CapabilityMode::Enforce, "network:127.0.0.1:0-65535");
        // Wrong host — refused before any connect is attempted, so this test
        // does no I/O at all.
        assert!(matches!(
            table.open_tcp_connect_checked(&policy, "169.254.169.254:80"),
            Err(FdCapabilityError::Denied(_))
        ));
        // A UDP socket with no bind address cannot name an endpoint, so the
        // narrow grant must refuse it (fail closed).
        assert!(matches!(
            table.open_udp_checked(&policy, None),
            Err(FdCapabilityError::Denied(_))
        ));
        // A loopback listener on an ephemeral port is inside the grant.
        let fd = table
            .open_tcp_listener_checked(&policy, "127.0.0.1:0")
            .expect("granted endpoint");
        table.close(fd).unwrap();
    }

    #[test]
    fn a_denial_maps_to_permission_denied_for_io_only_call_sites() {
        let table = FileDescriptorTable::new();
        let policy = caps(CapabilityMode::Enforce, "");
        let err = table
            .open_read_checked(&policy, "/anything")
            .expect_err("empty enforce set denies everything");
        let as_io: io::Error = err.into();
        assert_eq!(as_io.kind(), io::ErrorKind::PermissionDenied);
        assert!(as_io.to_string().contains("file-read"), "{as_io}");
    }

    // -----------------------------------------------------------------------
    // seek/position/size/truncate on `FileRead` + `FileWrite` entries.
    //
    // `FileChannel`s obtained from a `FileInputStream`/`FileOutputStream` are
    // backed by these two variants, and on Windows the JDK's
    // `FileChannelImpl.transferToDirect` calls `position()` (=> `rw_seek`) on
    // the source before every transfer. Accepting only `FileReadWrite` here
    // made `ExpandWar.copy` — and so all of `TestManagerWebapp` — fail with
    // `IOException: seek0: bad fd for rw_seek`.
    // -----------------------------------------------------------------------

    #[test]
    fn rw_seek_and_position_work_on_a_read_fd() {
        let path = temp_file_with("0123456789");
        let table = FileDescriptorTable::new();
        let fd = table.open_read(&path).unwrap();

        assert_eq!(table.rw_position(fd).unwrap(), 0);
        assert_eq!(table.rw_seek(fd, io::SeekFrom::Start(4)).unwrap(), 4);
        assert_eq!(table.rw_position(fd).unwrap(), 4);

        // The seek must be honoured by the following sequential read — i.e.
        // the BufReader's own buffer was reconciled, not left stale.
        let mut buf = [0u8; 3];
        assert_eq!(table.rw_read(fd, &mut buf).unwrap(), 3);
        assert_eq!(&buf, b"456");
        assert_eq!(table.rw_position(fd).unwrap(), 7);
        // ...and by `read_bytes`, which the `read0` native uses.
        let mut buf2 = [0u8; 3];
        assert_eq!(table.read_bytes(fd, &mut buf2).unwrap(), 3);
        assert_eq!(&buf2, b"789");

        assert_eq!(table.rw_seek(fd, io::SeekFrom::End(-2)).unwrap(), 8);
        assert_eq!(table.file_size(fd).unwrap(), 10);
        table.close(fd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rw_seek_position_and_size_work_on_a_write_fd() {
        let path = temp_path("seek_write_fd");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();

        assert_eq!(table.rw_position(fd).unwrap(), 0);
        table.write_bytes(fd, b"abcdef").unwrap();
        assert_eq!(table.rw_position(fd).unwrap(), 6);
        assert_eq!(table.file_size(fd).unwrap(), 6);

        // Re-seek and overwrite in place; the bytes must land at offset 2.
        assert_eq!(table.rw_seek(fd, io::SeekFrom::Start(2)).unwrap(), 2);
        assert_eq!(table.rw_write(fd, b"XY").unwrap(), 2);
        assert_eq!(table.rw_position(fd).unwrap(), 4);
        table.close(fd).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abXYef");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rw_set_length_truncates_a_write_fd() {
        let path = temp_path("truncate_write_fd");
        let table = FileDescriptorTable::new();
        let fd = table.open_write(&path, false).unwrap();
        table.write_bytes(fd, b"abcdefgh").unwrap();
        table.rw_set_length(fd, 3).unwrap();
        assert_eq!(table.file_size(fd).unwrap(), 3);
        table.close(fd).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"abc");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn rw_sync_accepts_a_write_fd() {
        let path = temp_path("sync_fds");
        let table = FileDescriptorTable::new();
        let wfd = table.open_write(&path, false).unwrap();
        table.write_bytes(wfd, b"durable").unwrap();
        table.rw_sync(wfd, true).unwrap();
        table.rw_sync(wfd, false).unwrap();
        table.close(wfd).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"durable");

        // A read-only handle stays rejected — see the comment on `rw_sync`.
        let rfd = table.open_read(&path).unwrap();
        assert!(table.rw_sync(rfd, false).is_err());
        table.close(rfd).unwrap();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn seek_family_still_rejects_non_file_fds() {
        let table = FileDescriptorTable::new();
        // stdout is not seekable — the error text callers match on must stay.
        let err = table.rw_seek(1, io::SeekFrom::Start(0)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
        assert!(err.to_string().contains("bad fd for rw_seek"), "{err}");
        assert!(table.rw_position(1).is_err());
        assert!(table.file_size(1).is_err());
        assert!(table.rw_set_length(1, 0).is_err());
        // ...and an fd that was never opened.
        let err = table.rw_seek(4242, io::SeekFrom::Start(0)).unwrap_err();
        assert!(err.to_string().contains("bad fd for rw_seek"), "{err}");
    }

    #[test]
    fn rw_read_and_rw_write_keep_their_direction() {
        let rpath = temp_file_with("data");
        let wpath = temp_path("direction_write");
        let table = FileDescriptorTable::new();
        let rfd = table.open_read(&rpath).unwrap();
        let wfd = table.open_write(&wpath, false).unwrap();

        assert!(table.rw_write(rfd, b"X").is_err());
        let mut buf = [0u8; 1];
        assert!(table.rw_read(wfd, &mut buf).is_err());

        table.close(rfd).unwrap();
        table.close(wfd).unwrap();
        let _ = fs::remove_file(&rpath);
        let _ = fs::remove_file(&wpath);
    }
}
