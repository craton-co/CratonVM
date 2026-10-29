// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The genuine `sun.nio.ch` natives the JDK's OWN channel, selector and
//! datagram bytecode reaches (SBR wave 5, lane `nioreal`).
//!
//! # Why these exist
//!
//! Under `--compatible` the `SocketChannel` / `ServerSocketChannel` /
//! `DatagramChannel` / `Selector` objects are native carriers
//! (`socket_channel.rs`, `nio_selector.rs`, `datagram.rs`): minted without
//! `<init>`, their state in side tables. Retiring those shadows under
//! `--jdk-only` hands the JDK's real `SocketChannelImpl`, `EPollSelectorImpl`,
//! `DatagramChannelImpl` bytecode the job, and that bytecode ends in the
//! `ACC_NATIVE` methods below -- the libnio surface. Every one of them was
//! either missing (`UnsatisfiedLinkError`) or a stub before this module
//! (`docs/feature-designs/profile-guided-inlining.md` section 3.3).
//!
//! # The fd model: ids stay virtual, the kernel sees the real descriptor
//!
//! `sun.nio.ch.Net` hands Java ids from `net_sockets()` (counting from
//! `0x4000_0000`), not OS descriptors, and the whole `java.net.Socket` path
//! already rides on that. A real `EPollSelectorImpl` passes those ids to
//! `EPoll.ctl`; [`nio_os_fd`] translates an id to the OS descriptor behind it
//! for the kernel while the epoll `data` word keeps the Java-visible id, so
//! `EPoll.getDescriptor` reads back exactly what `fdToKey` was keyed by. The
//! selector's own `EventFD` and epoll descriptors are real OS fds (small,
//! never in the id range) and pass through untranslated.
//!
//! # Datagrams
//!
//! A `socket0(.., stream = false, ..)` id owns a real UDP socket from birth
//! ([`net_socket0_datagram`]); bind, connect, disconnect, send, receive, the
//! connected read/write of `DatagramDispatcher`, multicast membership and the
//! multicast interface options all run on that one descriptor, as libnio's do.
//! Every blocking wait is close-aware (sliced `poll` + registry re-check), the
//! shape `net_read_close_aware` established for streams: a close from another
//! thread cannot take the OS handle away from a thread holding an `Arc` of it.
//!
//! Unix only. On other targets `socket0` keeps its old TCP-only behaviour and
//! the datagram natives report `UnsupportedOperationException`.

use super::*;

/// The Windows halves of the datagram, out-of-band and multicast natives (this file's `cfg(unix)` items are
/// their Linux twins), as a child module so they share this file's helpers.
#[cfg(windows)]
#[path = "net_nio_win.rs"]
mod win;

/// `IOUtil.drain(fd)` on Windows (the unix one is `nio_selector::ioutil_drain_native`).
#[cfg(windows)]
pub(crate) fn ioutil_drain_windows(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    win::ioutil_drain(ctx, args)
}

/// `IOStatus.UNAVAILABLE`.
const IOS_UNAVAILABLE: i32 = -2;
/// `IOStatus.INTERRUPTED`.
const IOS_INTERRUPTED: i32 = -3;
/// `IOStatus.EOF`.
const IOS_EOF: i32 = -1;

/// Largest datagram payload a receive stages at once (the UDP maximum).
const UDP_MAX_PAYLOAD: usize = 65_536;

/// `SOCKADDR_LEN` in `NativeSocketAddress`: `max(sizeof(sockaddr_in),
/// sizeof(sockaddr_in6))` on Linux.
const NATIVE_SOCKADDR_LEN: usize = 28;

/// The UDP socket behind a datagram id, cloned out of the registry.
pub(super) fn datagram_of(fd: i32) -> Option<Arc<std::net::UdpSocket>> {
    match net_sockets().read().get(&fd) {
        Some(NetSocketHandle::Datagram(u)) => Some(Arc::clone(u)),
        _ => None,
    }
}

/// Is `fd` still a live datagram id? `close_net_fd` swaps the entry for
/// `Closed`, so this flips exactly when Java closed it.
fn datagram_still_registered(fd: i32) -> bool {
    matches!(
        net_sockets().read().get(&fd),
        Some(NetSocketHandle::Datagram(_))
    )
}

// ---------------------------------------------------------------------------
// The OS descriptor behind an id (EPoll)
// ---------------------------------------------------------------------------

/// Keeps the socket an [`NioOsFd`] names alive for as long as the caller holds
/// it, so the raw descriptor cannot be closed and recycled mid-syscall.
#[allow(dead_code)] // held for its Drop, never read
enum NioOsKeep {
    Stream(Arc<TcpStream>),
    Listener(Arc<Mutex<TcpListener>>),
    Datagram(Arc<std::net::UdpSocket>),
}

/// An OS descriptor for a `net_sockets()` id, with the handle that keeps it
/// open.
pub(crate) struct NioOsFd {
    /// The kernel's descriptor (a `SOCKET` on Windows).
    pub(crate) raw: NioRaw,
    _keep: NioOsKeep,
}

/// The type of an OS socket handle: a file descriptor on Unix, a `SOCKET` on Windows.
#[cfg(unix)]
pub(crate) type NioRaw = i32;
#[cfg(not(unix))]
pub(crate) type NioRaw = usize;

/// The OS descriptor behind a `sun.nio.ch.Net` id, for `EPoll.ctl`.
///
/// `None` for an id with no OS socket (`Unbound`, `Closed`) or no id at all.
/// A listener's mutex can be held by an accept; only a blocking accept holds it
/// for long, and a blocking channel cannot be registered with a selector
/// (`IllegalBlockingModeException`), so `try_lock` first and wait only when
/// something non-blocking briefly holds it.
#[cfg(unix)]
pub(crate) fn nio_os_fd(id: i32) -> Option<NioOsFd> {
    use std::os::fd::AsRawFd;
    let keep = match net_sockets().read().get(&id) {
        Some(NetSocketHandle::Stream(s)) => NioOsKeep::Stream(Arc::clone(s)),
        Some(NetSocketHandle::Listener(l)) => NioOsKeep::Listener(Arc::clone(l)),
        Some(NetSocketHandle::Datagram(u)) => NioOsKeep::Datagram(Arc::clone(u)),
        _ => return None,
    };
    let raw = match &keep {
        NioOsKeep::Stream(s) => s.as_raw_fd(),
        NioOsKeep::Datagram(u) => u.as_raw_fd(),
        NioOsKeep::Listener(l) => match l.try_lock() {
            Some(g) => g.as_raw_fd(),
            None => l.lock().as_raw_fd(),
        },
    };
    Some(NioOsFd { raw, _keep: keep })
}

/// Windows: the `SOCKET` behind an id, for `WEPoll.ctl` / `WEPoll.wait`.
#[cfg(windows)]
pub(crate) fn nio_os_fd(id: i32) -> Option<NioOsFd> {
    use std::os::windows::io::AsRawSocket;
    let keep = match net_sockets().read().get(&id) {
        Some(NetSocketHandle::Stream(s)) => NioOsKeep::Stream(Arc::clone(s)),
        Some(NetSocketHandle::Listener(l)) => NioOsKeep::Listener(Arc::clone(l)),
        Some(NetSocketHandle::Datagram(u)) => NioOsKeep::Datagram(Arc::clone(u)),
        _ => return None,
    };
    let raw = match &keep {
        NioOsKeep::Stream(s) => s.as_raw_socket() as usize,
        NioOsKeep::Datagram(u) => u.as_raw_socket() as usize,
        NioOsKeep::Listener(l) => match l.try_lock() {
            Some(g) => g.as_raw_socket() as usize,
            None => l.lock().as_raw_socket() as usize,
        },
    };
    Some(NioOsFd { raw, _keep: keep })
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn nio_os_fd(_id: i32) -> Option<NioOsFd> {
    None
}

// ---------------------------------------------------------------------------
// Exceptions, libnio's way
// ---------------------------------------------------------------------------

/// A Java exception of class `class` (a `(String)V` or `()V` constructor).
/// Delegates to the crate's one builder, [`crate::libnio_error::throw_by_name`]
/// (round nio20 wave 2, lane L4): a constructor that throws is propagated, as
/// HotSpot's `JNU_ThrowByName` does, instead of being swallowed into the
/// `IOException` fallback.
fn throw_named(
    ctx: &mut dyn NativeContext,
    class: &str,
    message: Option<&str>,
) -> MethodCallFailed {
    crate::libnio_error::throw_by_name(ctx, class, message)
}

/// `strerror(errno)` without the `(os error N)` suffix std appends -- the text
/// libnio's `NET_ThrowByNameWithLastError` puts in the message.
pub(super) fn errno_text(errno: i32) -> String {
    crate::libnio_error::strerror(errno)
}

/// libnio's `handleSocketError`: the exception class an errno maps to.
#[cfg(unix)]
pub(super) fn errno_exception(ctx: &mut dyn NativeContext, errno: i32) -> MethodCallFailed {
    crate::libnio_error::handle_socket_error(ctx, errno)
}

/// Windows: the Winsock error code through the shared `io::Error` mapper.
#[cfg(windows)]
pub(super) fn errno_exception(ctx: &mut dyn NativeContext, errno: i32) -> MethodCallFailed {
    crate::libnio_error::handle_socket_io_error(ctx, &std::io::Error::from_raw_os_error(errno))
}

/// [`errno_exception`] with `suffix` appended to the message.
#[cfg(unix)]
fn errno_exception_with_suffix(
    ctx: &mut dyn NativeContext,
    errno: i32,
    suffix: &str,
) -> MethodCallFailed {
    let class = crate::libnio_error::socket_error_class(errno);
    let text = format!("{}{suffix}", errno_text(errno));
    throw_named(ctx, class, Some(&text))
}

#[cfg(unix)]
pub(super) fn last_errno() -> i32 {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

// ---------------------------------------------------------------------------
// sockaddr plumbing
// ---------------------------------------------------------------------------

/// Parse `read_inet_address_text`'s output (`a.b.c.d`, `x:y::z`, or
/// `x:y::z%<scope>`) into an address and a scope id.
fn parse_ip_text(text: &str) -> Option<(std::net::IpAddr, u32)> {
    let (ip, scope) = match text.split_once('%') {
        Some((ip, scope)) => (ip, scope.parse::<u32>().unwrap_or(0)),
        None => (text, 0),
    };
    let ip = ip.trim_start_matches('[').trim_end_matches(']');
    ip.parse::<std::net::IpAddr>().ok().map(|a| (a, scope))
}

/// The kernel address for `(ip, port)` on a socket of family `v6_socket`,
/// converted the way libnio's `NET_InetAddressToSockaddr` does for a
/// `preferIPv6` socket: an IPv4 address becomes v4-mapped, except the IPv4
/// wildcard, which becomes `::`. `Err(EAFNOSUPPORT)` for an IPv6 address on an
/// IPv4 socket.
#[cfg(unix)]
fn sockaddr_for(
    ip: std::net::IpAddr,
    scope: u32,
    port: i32,
    v6_socket: bool,
) -> Result<(libc::sockaddr_storage, libc::socklen_t), i32> {
    use std::net::IpAddr;
    let port = (port.clamp(0, 65535) as u16).to_be();
    // SAFETY: `sockaddr_storage` is plain old data; all-zero is a valid value.
    let mut storage: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    if v6_socket {
        let v6 = match ip {
            IpAddr::V6(v6) => v6,
            IpAddr::V4(v4) if v4.is_unspecified() => std::net::Ipv6Addr::UNSPECIFIED,
            IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        };
        // SAFETY: as above, all-zero is a valid `sockaddr_in6`.
        let mut sin6: libc::sockaddr_in6 = unsafe { std::mem::zeroed() };
        sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
        sin6.sin6_port = port;
        sin6.sin6_addr = libc::in6_addr {
            s6_addr: v6.octets(),
        };
        sin6.sin6_scope_id = scope;
        // SAFETY: `sockaddr_storage` is at least as large and as aligned as
        // `sockaddr_in6` by definition; the write stays inside `storage`.
        unsafe {
            std::ptr::write(
                (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in6>(),
                sin6,
            )
        };
        Ok((
            storage,
            std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
        ))
    } else {
        let v4 = match ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4,
                None => return Err(libc::EAFNOSUPPORT),
            },
        };
        // SAFETY: all-zero is a valid `sockaddr_in`.
        let mut sin: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_port = port;
        sin.sin_addr = libc::in_addr {
            s_addr: u32::from_ne_bytes(v4.octets()),
        };
        // SAFETY: `sockaddr_storage` is at least as large and as aligned as
        // `sockaddr_in`; the write stays inside `storage`.
        unsafe {
            std::ptr::write(
                (&mut storage as *mut libc::sockaddr_storage).cast::<libc::sockaddr_in>(),
                sin,
            )
        };
        Ok((
            storage,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ))
    }
}

/// Is this UDP socket AF_INET6? Read off the socket itself (`getsockname`
/// works on an unbound socket and reports its family).
#[cfg(unix)]
fn udp_is_v6(udp: &std::net::UdpSocket) -> bool {
    udp.local_addr().map(|a| a.is_ipv6()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Net.socket0 / bind0 / connect0 for datagrams
// ---------------------------------------------------------------------------

/// `socket0(preferIPv6, stream = false, reuse, _)`: a real UDP socket, set up
/// the way libnio's `Java_sun_nio_ch_Net_socket0` sets one up on Linux --
/// `IPV6_V6ONLY` off for a dual-stack socket, `SO_REUSEADDR` if asked, and
/// `IP_MULTICAST_ALL` off so a socket only sees the groups IT joined.
///
/// `None` on a target without `libc`: the caller keeps its old behaviour.
#[cfg(unix)]
pub(super) fn net_socket0_datagram(
    ctx: &mut dyn NativeContext,
    prefer_v6: bool,
    reuse: bool,
) -> Option<MethodCallResult> {
    use std::os::fd::FromRawFd;
    // `preferIPv6` is the JDK's own `Net.isIPv6Available()` answer. libnio
    // makes an AF_INET6 socket for it; a host whose kernel refuses one still
    // gets a working AF_INET socket rather than an exception.
    let open = |domain: libc::c_int| -> libc::c_int {
        // SAFETY: `socket` takes three integers and touches no caller memory.
        unsafe { libc::socket(domain, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) }
    };
    let mut v6 = prefer_v6;
    let mut raw = open(if v6 { libc::AF_INET6 } else { libc::AF_INET });
    if raw < 0 && v6 && last_errno() == libc::EAFNOSUPPORT {
        v6 = false;
        raw = open(libc::AF_INET);
    }
    if raw < 0 {
        let e = last_errno();
        return Some(Err(errno_exception(ctx, e)));
    }
    let set_int = |level: libc::c_int, opt: libc::c_int, val: libc::c_int| -> i32 {
        // SAFETY: the option value is a live `c_int` local and its exact size
        // is passed; `raw` is the descriptor just created above.
        unsafe {
            libc::setsockopt(
                raw,
                level,
                opt,
                (&val as *const libc::c_int).cast::<libc::c_void>(),
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        }
    };
    if v6 {
        let _ = set_int(libc::IPPROTO_IPV6, libc::IPV6_V6ONLY, 0);
    }
    if reuse {
        let _ = set_int(libc::SOL_SOCKET, libc::SO_REUSEADDR, 1);
    }
    #[cfg(target_os = "linux")]
    {
        // What HotSpot 25.0.4 does to a fresh datagram socket on Linux,
        // MEASURED with `strace -e setsockopt`: v6 -> option 49 at the IPv6
        // level off, IPV6_MULTICAST_HOPS 1 (the IPv4 TTL default), and
        // IPV6_MULTICAST_ALL off; v4 -> IP_MULTICAST_ALL off. Failures are not
        // fatal there either.
        if v6 {
            const IPV6_MULTICAST_ALL: libc::c_int = 29;
            let _ = set_int(libc::IPPROTO_IPV6, libc::IP_MULTICAST_ALL, 0);
            let _ = set_int(libc::IPPROTO_IPV6, libc::IPV6_MULTICAST_HOPS, 1);
            let _ = set_int(libc::IPPROTO_IPV6, IPV6_MULTICAST_ALL, 0);
        } else {
            let _ = set_int(libc::IPPROTO_IP, libc::IP_MULTICAST_ALL, 0);
        }
    }
    // SAFETY: `raw` is a fresh, owned descriptor nothing else refers to.
    let udp = unsafe { std::net::UdpSocket::from_raw_fd(raw) };
    let fd = register_handle(NetSocketHandle::Datagram(Arc::new(udp)));
    dbgnet!("socket0 datagram v6={v6} reuse={reuse} -> fd={fd:#x}");
    Some(Ok(Some(Value::Int(fd))))
}

#[cfg(windows)]
pub(super) fn net_socket0_datagram(
    ctx: &mut dyn NativeContext,
    prefer_v6: bool,
    reuse: bool,
) -> Option<MethodCallResult> {
    win::net_socket0_datagram(ctx, prefer_v6, reuse)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn net_socket0_datagram(
    _ctx: &mut dyn NativeContext,
    _prefer_v6: bool,
    _reuse: bool,
) -> Option<MethodCallResult> {
    None
}

/// `bind0` on a datagram id: `bind(2)` on the socket `socket0` created, so the
/// options already set on it (`SO_REUSEADDR` for a `MulticastSocket`) are in
/// force when the address is taken.
#[cfg(unix)]
pub(super) fn net_bind0_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    addr_text: &str,
    port: i32,
) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let Some((ip, scope)) = parse_ip_text(addr_text) else {
        return Err(throw_named(
            ctx,
            "java/net/SocketException",
            Some("Unresolved address"),
        ));
    };
    let (sa, len) = match sockaddr_for(ip, scope, port, udp_is_v6(udp)) {
        Ok(v) => v,
        Err(e) => return Err(errno_exception(ctx, e)),
    };
    // SAFETY: `sa`/`len` describe a valid sockaddr built above; the socket is
    // kept alive by the `Arc` the caller holds.
    let rc = unsafe {
        libc::bind(
            udp.as_raw_fd(),
            (&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc != 0 {
        let e = last_errno();
        return Err(errno_exception(ctx, e));
    }
    dbgnet!("bind0 datagram fd={fd:#x} {addr_text}:{port}");
    Ok(None)
}

#[cfg(windows)]
pub(super) fn net_bind0_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    addr_text: &str,
    port: i32,
) -> MethodCallResult {
    win::net_bind0_datagram(ctx, fd, udp, addr_text, port)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn net_bind0_datagram(
    _ctx: &mut dyn NativeContext,
    _fd: i32,
    _udp: &std::net::UdpSocket,
    _addr_text: &str,
    _port: i32,
) -> MethodCallResult {
    Err(unsupported("bind0 (datagram)"))
}

/// `connect0` on a datagram id: `connect(2)`, which only fixes the peer. The
/// outbound policy is consulted exactly as for a stream connect.
#[cfg(unix)]
pub(super) fn net_connect0_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    addr_text: &str,
    port: i32,
) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let target = join_host_port(addr_text, port);
    if let Err(reason) = crate::outbound_policy::check_outbound(&target) {
        return Err(ioex(format!("connect denied by outbound policy: {reason}")));
    }
    let Some((ip, scope)) = parse_ip_text(addr_text) else {
        return Err(throw_named(
            ctx,
            "java/net/SocketException",
            Some("Unresolved address"),
        ));
    };
    let (sa, len) = match sockaddr_for(ip, scope, port, udp_is_v6(udp)) {
        Ok(v) => v,
        Err(e) => return Err(errno_exception(ctx, e)),
    };
    // SAFETY: `sa`/`len` describe a valid sockaddr built above; the socket is
    // kept alive by the `Arc` the caller holds.
    let rc = unsafe {
        libc::connect(
            udp.as_raw_fd(),
            (&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc != 0 {
        let e = last_errno();
        return Err(errno_exception(ctx, e));
    }
    dbgnet!("connect0 datagram fd={fd:#x} {target}");
    Ok(Some(Value::Int(1)))
}

#[cfg(windows)]
pub(super) fn net_connect0_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    addr_text: &str,
    port: i32,
) -> MethodCallResult {
    win::net_connect0_datagram(ctx, fd, udp, addr_text, port)
}

#[cfg(not(any(unix, windows)))]
pub(super) fn net_connect0_datagram(
    _ctx: &mut dyn NativeContext,
    _fd: i32,
    _udp: &std::net::UdpSocket,
    _addr_text: &str,
    _port: i32,
) -> MethodCallResult {
    Err(unsupported("connect0 (datagram)"))
}

#[cfg(not(any(unix, windows)))]
fn unsupported(what: &str) -> MethodCallFailed {
    RuntimeError::UnsupportedOperationException {
        message: format!("{what}: not supported on this platform"),
    }
    .into()
}

// ---------------------------------------------------------------------------
// Close-aware waiting on a datagram socket
// ---------------------------------------------------------------------------

/// Slice length of a parked datagram wait, between registry re-checks. Same
/// role and value as `NET_READ_CLOSE_POLL_MS`.
const DGRAM_CLOSE_POLL_MS: i32 = 25;

/// Wait until `udp` is ready for `events`, `fd` is closed, or `timeout`
/// (poll(2) convention: negative forever, 0 probe) runs out.
/// `Ok(true)` ready or closed (the caller's next syscall/registry check says
/// which), `Ok(false)` timed out.
#[cfg(any(unix, windows))]
fn dgram_wait(
    fd: i32,
    udp: &std::net::UdpSocket,
    events: i32,
    timeout: i32,
) -> std::io::Result<bool> {
    dgram_wait_intr(fd, udp, events, timeout, None)
}

/// [`dgram_wait`], also returning `nio_interrupted_err` once `intr` reports
/// the thread interrupted (the interruptible-wait note in `net.rs`).
#[cfg(any(unix, windows))]
fn dgram_wait_intr(
    fd: i32,
    udp: &std::net::UdpSocket,
    events: i32,
    timeout: i32,
    intr: Option<&dyn Fn() -> bool>,
) -> std::io::Result<bool> {
    if timeout == 0 {
        return net_poll_raw(udp_raw(udp), events, 0);
    }
    let deadline =
        (timeout > 0).then(|| std::time::Instant::now() + Duration::from_millis(timeout as u64));
    loop {
        if !datagram_still_registered(fd) {
            return Ok(true);
        }
        if intr.is_some_and(|p| p()) {
            return Err(nio_interrupted_err());
        }
        let slice = match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Ok(false);
                }
                (remaining.as_millis() as i64).min(i64::from(DGRAM_CLOSE_POLL_MS)) as i32
            }
            None => DGRAM_CLOSE_POLL_MS,
        };
        if net_poll_raw(udp_raw(udp), events, slice)? {
            return Ok(true);
        }
    }
}

/// The OS handle of a UDP socket, in the type `net_poll_raw` takes.
#[cfg(unix)]
fn udp_raw(udp: &std::net::UdpSocket) -> NetRawHandle {
    use std::os::fd::AsRawFd;
    udp.as_raw_fd()
}

/// The OS handle of a UDP socket, in the type `net_poll_raw` takes.
#[cfg(windows)]
fn udp_raw(udp: &std::net::UdpSocket) -> NetRawHandle {
    use std::os::windows::io::AsRawSocket;
    udp.as_raw_socket() as NetRawHandle
}

/// `Net.poll` on a datagram id (`DatagramChannelImpl.park`, the adaptor's
/// timed receive). Returns 1 when ready (or closed underneath), 0 on timeout.
#[cfg(any(unix, windows))]
pub(super) fn net_poll_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    events: i32,
    timeout_millis: i64,
) -> MethodCallResult {
    let timeout = if timeout_millis < 0 {
        -1
    } else {
        timeout_millis.min(i64::from(i32::MAX)) as i32
    };
    if timeout != 0 {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    let r = dgram_wait(fd, udp, events, timeout);
    if timeout != 0 {
        ctx.end_blocking_region();
    }
    match r {
        Ok(ready) => Ok(Some(Value::Int(i32::from(ready)))),
        Err(e) => Err(nio_net_err(ctx, "poll", e)),
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) fn net_poll_datagram(
    _ctx: &mut dyn NativeContext,
    _fd: i32,
    _udp: &std::net::UdpSocket,
    _events: i32,
    _timeout_millis: i64,
) -> MethodCallResult {
    Ok(Some(Value::Int(0)))
}

// ---------------------------------------------------------------------------
// DatagramChannelImpl.send0 / receive0 / disconnect0
// ---------------------------------------------------------------------------

fn long_arg(args: &[Value], idx: usize) -> i64 {
    match args.get(idx) {
        Some(Value::Long(v)) => *v,
        Some(Value::Int(v)) => i64::from(*v),
        _ => 0,
    }
}

/// The datagram id a `FileDescriptor` argument names, or the IOException the
/// JDK would see for a descriptor that is not one (EBADF/ENOTSOCK).
#[cfg(any(unix, windows))]
fn datagram_arg(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
    what: &str,
) -> Result<(i32, Arc<std::net::UdpSocket>), MethodCallFailed> {
    let fd_obj = obj_arg(args, idx)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex(format!("{what}: FileDescriptor has no fd id")))?;
    #[cfg(unix)]
    let ebadf = errno_text(libc::EBADF);
    #[cfg(windows)]
    let ebadf = "Bad file descriptor".to_string();
    match datagram_of(fd) {
        Some(u) => Ok((fd, u)),
        None => Err(throw_named(ctx, "java/net/SocketException", Some(&ebadf))),
    }
}

/// `receive0(fd, address, len, senderAddress, connected) -> int`.
///
/// libnio: `recvfrom` into `address`, the sender's `sockaddr` into
/// `senderAddress` (a `NativeSocketAddress` block Java decodes), EAGAIN ->
/// `UNAVAILABLE`, EINTR -> `INTERRUPTED`, ECONNREFUSED retried when not
/// connected and `PortUnreachableException` when connected.
#[cfg(unix)]
fn dc_receive0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let (fd, udp) = datagram_arg(ctx, args, 0, "receive0")?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2).clamp(0, UDP_MAX_PAYLOAD as i32) as usize;
    let sender = long_arg(args, 3);
    let connected = bool_arg(args, 4);
    let can_park = !net_fd_is_nonblocking(fd);
    let interruptible = can_park && nio_blocker_armed(ctx);
    let mut buf = vec![0u8; len.max(1)];
    loop {
        if can_park {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let r = if interruptible {
                let probe = || ctx.is_interrupted(false);
                dgram_wait_intr(fd, &udp, NET_POLLIN, -1, Some(&probe))
            } else {
                dgram_wait(fd, &udp, NET_POLLIN, -1)
            };
            ctx.end_blocking_region();
            match r {
                Err(e) if is_nio_interrupted(&e) => {
                    nio_deliver_interrupt(ctx);
                    return Ok(Some(Value::Int(IOS_INTERRUPTED)));
                }
                Err(e) => return Err(nio_net_err(ctx, "receive0", e)),
                Ok(_) => {}
            }
            if !datagram_still_registered(fd) {
                // Closed underneath: `endRead` turns this into
                // `AsynchronousCloseException` because the channel is no
                // longer open.
                return Ok(Some(Value::Int(IOS_UNAVAILABLE)));
            }
        }
        // SAFETY: all-zero is a valid `sockaddr_storage`.
        let mut from: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
        let mut from_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        let flags = if can_park { libc::MSG_DONTWAIT } else { 0 };
        // SAFETY: `buf` is writable for `len` bytes and `from`/`from_len` are a
        // valid out-pair; the socket is kept alive by `udp`.
        let n = unsafe {
            libc::recvfrom(
                udp.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                len,
                flags,
                (&mut from as *mut libc::sockaddr_storage).cast::<libc::sockaddr>(),
                &mut from_len,
            )
        };
        if n < 0 {
            let e = last_errno();
            match e {
                libc::EAGAIN => {
                    if can_park {
                        continue;
                    }
                    return Ok(Some(Value::Int(IOS_UNAVAILABLE)));
                }
                libc::EINTR => return Ok(Some(Value::Int(IOS_INTERRUPTED))),
                libc::ECONNREFUSED if !connected => continue,
                libc::ECONNREFUSED => {
                    return Err(throw_named(ctx, "java/net/PortUnreachableException", None))
                }
                _ => return Err(errno_exception(ctx, e)),
            }
        }
        let n = n as usize;
        if n > 0 && !ctx.copy_to_native_memory(addr, &buf[..n]) {
            return Err(ioex(format!(
                "receive0: invalid destination address {addr:#x}"
            )));
        }
        // The whole `NativeSocketAddress` block: a v4 sender leaves the tail
        // zero rather than whatever the previous datagram's sender left.
        // SAFETY: `from` is a live, fully initialised local of at least
        // NATIVE_SOCKADDR_LEN bytes.
        let sa_bytes = unsafe {
            std::slice::from_raw_parts(
                (&from as *const libc::sockaddr_storage).cast::<u8>(),
                NATIVE_SOCKADDR_LEN,
            )
        };
        if !ctx.copy_to_native_memory(sender, sa_bytes) {
            return Err(ioex(format!(
                "receive0: invalid sender address {sender:#x}"
            )));
        }
        return Ok(Some(Value::Int(n as i32)));
    }
}

/// `send0(fd, address, len, targetAddress, targetAddressLen) -> int`.
///
/// libnio: `sendto` with the `sockaddr` `NativeSocketAddress.encode` wrote.
/// EAGAIN -> `UNAVAILABLE`, EINTR -> `INTERRUPTED`, ECONNREFUSED ->
/// `PortUnreachableException`.
#[cfg(unix)]
fn dc_send0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let (fd, udp) = datagram_arg(ctx, args, 0, "send0")?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2).clamp(0, UDP_MAX_PAYLOAD as i32) as usize;
    let target = long_arg(args, 3);
    let target_len =
        int_arg(args, 4).clamp(0, std::mem::size_of::<libc::sockaddr_storage>() as i32) as usize;
    let mut data = vec![0u8; len];
    if len > 0 && !ctx.copy_from_native_memory(addr, &mut data) {
        return Err(ioex(format!("send0: invalid source address {addr:#x}")));
    }
    // SAFETY: all-zero is a valid `sockaddr_storage`.
    let mut to: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    {
        // SAFETY: `to` is a live local of `size_of::<sockaddr_storage>()` bytes
        // and `target_len` is clamped to that size.
        let to_bytes = unsafe {
            std::slice::from_raw_parts_mut(
                (&mut to as *mut libc::sockaddr_storage).cast::<u8>(),
                target_len,
            )
        };
        if !ctx.copy_from_native_memory(target, to_bytes) {
            return Err(ioex(format!("send0: invalid target address {target:#x}")));
        }
    }
    let can_park = !net_fd_is_nonblocking(fd);
    if can_park {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    // SAFETY: `data` is readable for `len` bytes, `to` holds `target_len`
    // bytes of sockaddr, and the socket is kept alive by `udp`.
    let n = unsafe {
        libc::sendto(
            udp.as_raw_fd(),
            data.as_ptr().cast::<libc::c_void>(),
            len,
            0,
            (&to as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            target_len as libc::socklen_t,
        )
    };
    let e = if n < 0 { last_errno() } else { 0 };
    if can_park {
        ctx.end_blocking_region();
    }
    if n < 0 {
        return match e {
            libc::EAGAIN => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
            libc::EINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
            libc::ECONNREFUSED => Err(throw_named(ctx, "java/net/PortUnreachableException", None)),
            _ => Err(errno_exception(ctx, e)),
        };
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `disconnect0(fd, isIPv6)`: `connect(2)` with `AF_UNSPEC`, as libnio does.
#[cfg(unix)]
fn dc_disconnect0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let (_fd, udp) = datagram_arg(ctx, args, 0, "disconnect0")?;
    let is_v6 = bool_arg(args, 1);
    // SAFETY: all-zero is a valid `sockaddr_storage`; its family field is
    // then AF_UNSPEC (0) already.
    let sa: libc::sockaddr_storage = unsafe { std::mem::zeroed() };
    let len = if is_v6 {
        std::mem::size_of::<libc::sockaddr_in6>()
    } else {
        std::mem::size_of::<libc::sockaddr_in>()
    } as libc::socklen_t;
    // SAFETY: `sa` is a zeroed sockaddr of at least `len` bytes; the socket is
    // kept alive by `udp`.
    let rc = unsafe {
        libc::connect(
            udp.as_raw_fd(),
            (&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            len,
        )
    };
    if rc != 0 {
        let e = last_errno();
        // Linux reports EAFNOSUPPORT for an AF_UNSPEC connect on some kernels
        // after it has already dissolved the association.
        if e != libc::EAFNOSUPPORT {
            return Err(errno_exception(ctx, e));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// DatagramDispatcher (connected datagram read/write) and dup0
// ---------------------------------------------------------------------------

/// One `recv` on a connected datagram socket (`DatagramDispatcher.read0`):
/// ECONNREFUSED -> `PortUnreachableException`, a zero-length datagram ->
/// `EOF` (libnio's `convertReturnVal(.., reading = true)`).
#[cfg(unix)]
fn dgram_recv_into(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    buf: &mut [u8],
) -> Result<i32, MethodCallFailed> {
    use std::os::fd::AsRawFd;
    let can_park = !net_fd_is_nonblocking(fd);
    let interruptible = can_park && nio_blocker_armed(ctx);
    loop {
        if can_park {
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let r = if interruptible {
                let probe = || ctx.is_interrupted(false);
                dgram_wait_intr(fd, udp, NET_POLLIN, -1, Some(&probe))
            } else {
                dgram_wait(fd, udp, NET_POLLIN, -1)
            };
            ctx.end_blocking_region();
            match r {
                Err(e) if is_nio_interrupted(&e) => {
                    nio_deliver_interrupt(ctx);
                    return Ok(IOS_INTERRUPTED);
                }
                other => {
                    other.map_err(|e| {
                        crate::libnio_error::io_exception_with_last_error(&e, "Read failed")
                    })?;
                }
            }
            if !datagram_still_registered(fd) {
                return Ok(IOS_UNAVAILABLE);
            }
        }
        let flags = if can_park { libc::MSG_DONTWAIT } else { 0 };
        // SAFETY: `buf` is writable for its length; the socket is kept alive
        // by `udp`.
        let n = unsafe {
            libc::recv(
                udp.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                flags,
            )
        };
        if n < 0 {
            let e = last_errno();
            return match e {
                libc::EAGAIN if can_park => continue,
                libc::EAGAIN => Ok(IOS_UNAVAILABLE),
                libc::EINTR => Ok(IOS_INTERRUPTED),
                libc::ECONNREFUSED => {
                    Err(throw_named(ctx, "java/net/PortUnreachableException", None))
                }
                _ => Err(errno_exception(ctx, e)),
            };
        }
        return Ok(if n == 0 { IOS_EOF } else { n as i32 });
    }
}

/// One `send` on a connected datagram socket (`DatagramDispatcher.write0`).
#[cfg(unix)]
fn dgram_send_from(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    data: &[u8],
) -> Result<i32, MethodCallFailed> {
    use std::os::fd::AsRawFd;
    let can_park = !net_fd_is_nonblocking(fd);
    if can_park {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    // SAFETY: `data` is readable for its length; the socket is kept alive by
    // `udp`.
    let n = unsafe {
        libc::send(
            udp.as_raw_fd(),
            data.as_ptr().cast::<libc::c_void>(),
            data.len(),
            0,
        )
    };
    let e = if n < 0 { last_errno() } else { 0 };
    if can_park {
        ctx.end_blocking_region();
    }
    if n < 0 {
        return match e {
            libc::EAGAIN => Ok(IOS_UNAVAILABLE),
            libc::EINTR => Ok(IOS_INTERRUPTED),
            libc::ECONNREFUSED => Err(throw_named(ctx, "java/net/PortUnreachableException", None)),
            _ => Err(errno_exception(ctx, e)),
        };
    }
    Ok(n as i32)
}

/// `DatagramDispatcher.read0(fd, address, len) -> int`.
#[cfg(unix)]
fn dd_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd, udp) = datagram_arg(ctx, args, 0, "read0")?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2).clamp(0, UDP_MAX_PAYLOAD as i32) as usize;
    let mut buf = vec![0u8; len.max(1)];
    let n = dgram_recv_into(ctx, fd, &udp, &mut buf[..len.max(1)])?;
    if n > 0 && !ctx.copy_to_native_memory(addr, &buf[..n as usize]) {
        return Err(ioex(format!(
            "read0: invalid destination address {addr:#x}"
        )));
    }
    Ok(Some(Value::Int(n)))
}

/// `DatagramDispatcher.write0(fd, address, len) -> int`.
#[cfg(unix)]
fn dd_write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd, udp) = datagram_arg(ctx, args, 0, "write0")?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2).clamp(0, UDP_MAX_PAYLOAD as i32) as usize;
    let mut data = vec![0u8; len];
    if len > 0 && !ctx.copy_from_native_memory(addr, &mut data) {
        return Err(ioex(format!("write0: invalid source address {addr:#x}")));
    }
    let n = dgram_send_from(ctx, fd, &udp, &data)?;
    Ok(Some(Value::Int(n)))
}

/// `DatagramDispatcher.readv0(fd, iovAddress, iovCount) -> long`.
#[cfg(unix)]
fn dd_readv0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd, udp) = datagram_arg(ctx, args, 0, "readv0")?;
    let iov = read_iovecs(ctx, long_arg(args, 1), int_arg(args, 2))?;
    let total: usize = iov
        .iter()
        .map(|(_, l)| *l)
        .sum::<usize>()
        .min(UDP_MAX_PAYLOAD);
    let mut buf = vec![0u8; total.max(1)];
    let n = dgram_recv_into(ctx, fd, &udp, &mut buf[..total.max(1)])?;
    if n > 0 {
        scatter(ctx, &iov, &buf[..n as usize])?;
    }
    Ok(Some(Value::Long(i64::from(n))))
}

/// `DatagramDispatcher.writev0(fd, iovAddress, iovCount) -> long`.
#[cfg(unix)]
fn dd_writev0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd, udp) = datagram_arg(ctx, args, 0, "writev0")?;
    let iov = read_iovecs(ctx, long_arg(args, 1), int_arg(args, 2))?;
    let data = gather(ctx, &iov, UDP_MAX_PAYLOAD)?;
    let n = dgram_send_from(ctx, fd, &udp, &data)?;
    Ok(Some(Value::Long(i64::from(n))))
}

/// `DatagramDispatcher.dup0(fd1, fd2)`: libnio `dup2(fd1, fd2)` -- `fd2`
/// thereafter names `fd1`'s socket and its old socket is closed.
/// (`DatagramChannelImpl.repairSocket` after a Linux disconnect.) At the id
/// level: `fd2` gets a `dup` of `fd1`'s socket, and inherits its mode.
#[cfg(unix)]
fn dd_dup0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd1, udp1) = datagram_arg(ctx, args, 0, "dup0")?;
    let fd2_obj = obj_arg(args, 1)?;
    let fd2 = net_fd_from_descriptor(ctx, fd2_obj)
        .ok_or_else(|| ioex("dup0: FileDescriptor has no fd id"))?;
    let dup = udp1
        .try_clone()
        .map_err(|e| crate::libnio_error::io_exception_with_last_error(&e, "dup2 failed"))?;
    net_sockets()
        .write()
        .insert(fd2, NetSocketHandle::Datagram(Arc::new(dup)));
    let mode = net_pending_nonblocking().read().get(&fd1).copied();
    match mode {
        Some(nb) => {
            net_pending_nonblocking().write().insert(fd2, nb);
        }
        None => {
            net_pending_nonblocking().write().remove(&fd2);
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// iovec plumbing (SocketDispatcher / DatagramDispatcher readv0 / writev0)
// ---------------------------------------------------------------------------

/// The `struct iovec` array `IOVecWrapper` built at `address`: `(base, len)`
/// pairs, `base` a native address (raw pointer or arena handle).
pub(crate) fn read_iovecs(
    ctx: &mut dyn NativeContext,
    address: i64,
    count: i32,
) -> Result<Vec<(i64, usize)>, MethodCallFailed> {
    let count = count.clamp(0, 1024) as usize;
    let mut raw = vec![0u8; count * 16];
    if count > 0 && !ctx.copy_from_native_memory(address, &mut raw) {
        return Err(ioex(format!(
            "readv/writev: invalid iovec address {address:#x}"
        )));
    }
    Ok(raw
        .chunks_exact(16)
        .map(|c| {
            let base = i64::from_ne_bytes(c[0..8].try_into().unwrap_or([0; 8]));
            let len = u64::from_ne_bytes(c[8..16].try_into().unwrap_or([0; 8]));
            (base, len.min(NET_MAX_TRANSFER as u64) as usize)
        })
        .collect())
}

/// Copy `bytes` out across `iov` in order, the way `readv(2)` fills it.
pub(crate) fn scatter(
    ctx: &mut dyn NativeContext,
    iov: &[(i64, usize)],
    bytes: &[u8],
) -> Result<(), MethodCallFailed> {
    let mut off = 0usize;
    for (base, len) in iov {
        if off >= bytes.len() {
            break;
        }
        let take = (*len).min(bytes.len() - off);
        if take > 0 && !ctx.copy_to_native_memory(*base, &bytes[off..off + take]) {
            return Err(ioex(format!(
                "readv: invalid destination address {base:#x}"
            )));
        }
        off += take;
    }
    Ok(())
}

/// Concatenate `iov`'s buffers (at most `cap` bytes), the way `writev(2)`
/// reads them.
pub(crate) fn gather(
    ctx: &mut dyn NativeContext,
    iov: &[(i64, usize)],
    cap: usize,
) -> Result<Vec<u8>, MethodCallFailed> {
    let total: usize = iov.iter().map(|(_, l)| *l).sum::<usize>().min(cap);
    let mut data = vec![0u8; total];
    let mut off = 0usize;
    for (base, len) in iov {
        if off >= total {
            break;
        }
        let take = (*len).min(total - off);
        if take > 0 && !ctx.copy_from_native_memory(*base, &mut data[off..off + take]) {
            return Err(ioex(format!("writev: invalid source address {base:#x}")));
        }
        off += take;
    }
    Ok(data)
}

/// `SocketDispatcher.readv0(fd, iovAddress, iovCount) -> long`: ONE read of up
/// to the iovecs' total, through the same `net_stream_read` `read0` uses,
/// scattered across the buffers in order.
fn sd_readv0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("readv0: FileDescriptor has no fd id"))?;
    let iov = read_iovecs(ctx, long_arg(args, 1), int_arg(args, 2))?;
    let total: usize = iov
        .iter()
        .map(|(_, l)| *l)
        .sum::<usize>()
        .min(NET_MAX_TRANSFER as usize);
    if total == 0 {
        return Ok(Some(Value::Long(0)));
    }
    let r = net_stream_read(ctx, fd, total, None, None, &mut |ctx, bytes| {
        scatter(ctx, &iov, bytes)
    })?;
    Ok(Some(Value::Long(match r {
        Some(Value::Int(n)) => i64::from(n),
        _ => 0,
    })))
}

/// `SocketDispatcher.writev0(fd, iovAddress, iovCount) -> long`: the buffers
/// gathered, then ONE write through `net_stream_write`.
fn sd_writev0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("writev0: FileDescriptor has no fd id"))?;
    let iov = read_iovecs(ctx, long_arg(args, 1), int_arg(args, 2))?;
    let data = gather(ctx, &iov, NET_MAX_TRANSFER as usize)?;
    if data.is_empty() {
        return Ok(Some(Value::Long(0)));
    }
    let r = net_stream_write(ctx, fd, &data);
    deliver_pending_nio_interrupt(ctx);
    let r = r?;
    Ok(Some(Value::Long(match r {
        Some(Value::Int(n)) => i64::from(n),
        _ => 0,
    })))
}

// ---------------------------------------------------------------------------
// Net.pollConnect / sendOOB / discardOOB
// ---------------------------------------------------------------------------

/// `Net.pollConnect(fd, timeout) -> boolean`: libnio polls for `POLLOUT` and
/// then reads `SO_ERROR`, throwing the connect failure it finds.
///
/// `connect0` here completes the connection before it returns (it is a
/// blocking connect under the outbound policy), so the JDK only reaches this
/// for a channel that is already connected -- `finishConnect` on a pending
/// connect never happens. A connected stream answers `true` after the same
/// `SO_ERROR` check; anything else has no pending connection and answers
/// `false`.
#[cfg(unix)]
fn net_poll_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("pollConnect: FileDescriptor has no fd id"))?;
    let stream = match net_sockets().read().get(&fd) {
        Some(NetSocketHandle::Stream(s)) => Arc::clone(s),
        _ => return Ok(Some(Value::Int(0))),
    };
    let timeout = long_arg(args, 1).clamp(-1, i64::from(i32::MAX)) as i32;
    if timeout != 0 {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    let ready = net_poll_raw(stream.as_raw_fd(), NET_POLLOUT, timeout);
    if timeout != 0 {
        ctx.end_blocking_region();
    }
    match ready {
        Ok(true) => {}
        Ok(false) => return Ok(Some(Value::Int(0))),
        Err(e) => return Err(nio_net_err(ctx, "pollConnect", e)),
    }
    match stream.take_error() {
        Ok(None) => Ok(Some(Value::Int(1))),
        Ok(Some(e)) => {
            // HotSpot's `Net.pollConnect` words a failed connect
            // `<strerror> (connect failed)` (measured: a non-blocking
            // `SocketChannel` to a closed loopback port, `finishConnect`).
            let errno = e.raw_os_error().unwrap_or(libc::ECONNREFUSED);
            Err(errno_exception_with_suffix(ctx, errno, " (connect failed)"))
        }
        Err(e) => Err(nio_net_err(ctx, "pollConnect", e)),
    }
}

/// `Net.sendOOB(fd, byte) -> int`: `send(.., MSG_OOB)`.
#[cfg(unix)]
fn net_send_oob(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("sendOOB: FileDescriptor has no fd id"))?;
    let stream = match net_sockets().read().get(&fd) {
        Some(NetSocketHandle::Stream(s)) => Arc::clone(s),
        _ => {
            return Err(throw_named(
                ctx,
                "java/net/SocketException",
                Some("Socket is not connected"),
            ))
        }
    };
    let b = [int_arg(args, 1) as u8];
    // SAFETY: `b` is a one-byte local; the socket is kept alive by `stream`.
    let n = unsafe {
        libc::send(
            stream.as_raw_fd(),
            b.as_ptr().cast::<libc::c_void>(),
            1,
            libc::MSG_OOB,
        )
    };
    if n < 0 {
        let e = last_errno();
        return match e {
            libc::EAGAIN => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
            libc::EINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
            _ => Err(errno_exception(ctx, e)),
        };
    }
    Ok(Some(Value::Int(n as i32)))
}

/// `Net.discardOOB(fd) -> boolean`: drain pending urgent data.
#[cfg(unix)]
fn net_discard_oob(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::os::fd::AsRawFd;
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("discardOOB: FileDescriptor has no fd id"))?;
    let stream = match net_sockets().read().get(&fd) {
        Some(NetSocketHandle::Stream(s)) => Arc::clone(s),
        _ => return Ok(Some(Value::Int(0))),
    };
    let mut discarded = false;
    let mut buf = [0u8; 8];
    loop {
        // SAFETY: `buf` is a writable local; the socket is kept alive by `stream`.
        let n = unsafe {
            libc::recv(
                stream.as_raw_fd(),
                buf.as_mut_ptr().cast::<libc::c_void>(),
                buf.len(),
                libc::MSG_OOB | libc::MSG_DONTWAIT,
            )
        };
        if n > 0 {
            discarded = true;
            continue;
        }
        if n == 0 {
            break;
        }
        let e = last_errno();
        if e == libc::EINTR {
            continue;
        }
        if e != libc::EAGAIN && e != libc::EINVAL {
            return Err(errno_exception(ctx, e));
        }
        break;
    }
    Ok(Some(Value::Int(i32::from(discarded))))
}

// ---------------------------------------------------------------------------
// Multicast membership and interface
// ---------------------------------------------------------------------------

/// The raw descriptor of a datagram `FileDescriptor` argument, with its
/// keep-alive.
#[cfg(unix)]
fn dgram_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
    what: &str,
) -> Result<(i32, Arc<std::net::UdpSocket>), MethodCallFailed> {
    use std::os::fd::AsRawFd;
    let (_fd, udp) = datagram_arg(ctx, args, idx, what)?;
    Ok((udp.as_raw_fd(), udp))
}

#[cfg(unix)]
fn setsockopt_bytes(raw: i32, level: libc::c_int, opt: libc::c_int, val: &[u8]) -> Result<(), i32> {
    // SAFETY: `val` is readable for exactly the length passed.
    let rc = unsafe {
        libc::setsockopt(
            raw,
            level,
            opt,
            val.as_ptr().cast::<libc::c_void>(),
            val.len() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(last_errno())
    }
}

/// `Net.joinOrDrop4(join, fd, group, interf, source) -> int`: libnio's
/// `IP_ADD_MEMBERSHIP`/`IP_DROP_MEMBERSHIP` (`struct ip_mreq`) or, with a
/// source, `IP_ADD_SOURCE_MEMBERSHIP`/`IP_DROP_SOURCE_MEMBERSHIP`
/// (`struct ip_mreq_source`, Linux field order: group, interface, source).
/// A join the kernel cannot do source-specifically answers `UNAVAILABLE`.
#[cfg(unix)]
fn net_join_or_drop4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let join = bool_arg(args, 0);
    let (raw, _keep) = dgram_raw(ctx, args, 1, "joinOrDrop4")?;
    let group = (int_arg(args, 2) as u32).to_be_bytes();
    let interf = (int_arg(args, 3) as u32).to_be_bytes();
    let source = int_arg(args, 4);
    let r = if source == 0 {
        let mut mreq = [0u8; 8];
        mreq[0..4].copy_from_slice(&group);
        mreq[4..8].copy_from_slice(&interf);
        let opt = if join {
            libc::IP_ADD_MEMBERSHIP
        } else {
            libc::IP_DROP_MEMBERSHIP
        };
        setsockopt_bytes(raw, libc::IPPROTO_IP, opt, &mreq)
    } else {
        let mut mreq = [0u8; 12];
        mreq[0..4].copy_from_slice(&group);
        mreq[4..8].copy_from_slice(&interf);
        mreq[8..12].copy_from_slice(&(source as u32).to_be_bytes());
        let opt = if join {
            libc::IP_ADD_SOURCE_MEMBERSHIP
        } else {
            libc::IP_DROP_SOURCE_MEMBERSHIP
        };
        setsockopt_bytes(raw, libc::IPPROTO_IP, opt, &mreq)
    };
    match r {
        Ok(()) => Ok(Some(Value::Int(0))),
        Err(e) if join && (e == libc::ENOPROTOOPT || e == libc::EOPNOTSUPP) => {
            Ok(Some(Value::Int(IOS_UNAVAILABLE)))
        }
        Err(e) => Err(errno_exception(ctx, e)),
    }
}

/// `Net.blockOrUnblock4(block, fd, group, interf, source) -> int`.
#[cfg(unix)]
fn net_block_or_unblock4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let block = bool_arg(args, 0);
    let (raw, _keep) = dgram_raw(ctx, args, 1, "blockOrUnblock4")?;
    let mut mreq = [0u8; 12];
    mreq[0..4].copy_from_slice(&(int_arg(args, 2) as u32).to_be_bytes());
    mreq[4..8].copy_from_slice(&(int_arg(args, 3) as u32).to_be_bytes());
    mreq[8..12].copy_from_slice(&(int_arg(args, 4) as u32).to_be_bytes());
    let opt = if block {
        libc::IP_BLOCK_SOURCE
    } else {
        libc::IP_UNBLOCK_SOURCE
    };
    match setsockopt_bytes(raw, libc::IPPROTO_IP, opt, &mreq) {
        Ok(()) => Ok(Some(Value::Int(0))),
        Err(e) if block && (e == libc::ENOPROTOOPT || e == libc::EOPNOTSUPP) => {
            Ok(Some(Value::Int(IOS_UNAVAILABLE)))
        }
        Err(e) => Err(errno_exception(ctx, e)),
    }
}

/// A Java `byte[16]` argument, or `None` for null.
fn byte16_arg(ctx: &mut dyn NativeContext, args: &[Value], idx: usize) -> Option<[u8; 16]> {
    let Some(Value::Object(Some(arr))) = args.get(idx).copied() else {
        return None;
    };
    if ctx.array_length(arr) < 16 {
        return None;
    }
    let mut out = [0u8; 16];
    (ctx.read_byte_array_into(arr, 0, &mut out) == 16).then_some(out)
}

/// `Net.joinOrDrop6(join, fd, group, index, source) -> int`: libnio's
/// `IPV6_ADD_MEMBERSHIP`/`IPV6_DROP_MEMBERSHIP` (`struct ipv6_mreq`). A
/// source-specific IPv6 join (`MCAST_JOIN_SOURCE_GROUP`) answers
/// `UNAVAILABLE`, which the JDK reports as unsupported -- not implemented here.
#[cfg(unix)]
fn net_join_or_drop6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let join = bool_arg(args, 0);
    let (raw, _keep) = dgram_raw(ctx, args, 1, "joinOrDrop6")?;
    let Some(group) = byte16_arg(ctx, args, 2) else {
        return Err(RuntimeError::NullPointerException { message: None }.into());
    };
    let index = int_arg(args, 3);
    if byte16_arg(ctx, args, 4).is_some() {
        return Ok(Some(Value::Int(IOS_UNAVAILABLE)));
    }
    let mut mreq = [0u8; 20];
    mreq[0..16].copy_from_slice(&group);
    mreq[16..20].copy_from_slice(&index.to_ne_bytes());
    let opt = if join {
        libc::IPV6_ADD_MEMBERSHIP
    } else {
        libc::IPV6_DROP_MEMBERSHIP
    };
    match setsockopt_bytes(raw, libc::IPPROTO_IPV6, opt, &mreq) {
        Ok(()) => Ok(Some(Value::Int(0))),
        Err(e) if join && (e == libc::ENOPROTOOPT || e == libc::EOPNOTSUPP) => {
            Ok(Some(Value::Int(IOS_UNAVAILABLE)))
        }
        Err(e) => Err(errno_exception(ctx, e)),
    }
}

/// `Net.blockOrUnblock6(..) -> int`: source filtering on IPv6 is not modelled;
/// `UNAVAILABLE` is the JDK's own "not supported" answer.
#[cfg(unix)]
fn net_block_or_unblock6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let _ = dgram_raw(ctx, args, 1, "blockOrUnblock6")?;
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

/// `Net.setInterface4(fd, interf)`: `IP_MULTICAST_IF` with a `struct in_addr`.
#[cfg(unix)]
fn net_set_interface4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "setInterface4")?;
    let addr = (int_arg(args, 1) as u32).to_be_bytes();
    setsockopt_bytes(raw, libc::IPPROTO_IP, libc::IP_MULTICAST_IF, &addr)
        .map_err(|e| errno_exception(ctx, e))?;
    Ok(None)
}

/// `Net.getInterface4(fd) -> int`.
#[cfg(unix)]
fn net_get_interface4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "getInterface4")?;
    let mut buf = [0u8; 4];
    let mut len = buf.len() as libc::socklen_t;
    // SAFETY: `buf`/`len` are a valid out-pair; `raw` is kept alive by `_keep`.
    let rc = unsafe {
        libc::getsockopt(
            raw,
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        let e = last_errno();
        return Err(errno_exception(ctx, e));
    }
    Ok(Some(Value::Int(u32::from_be_bytes(buf) as i32)))
}

/// `Net.setInterface6(fd, index)`: `IPV6_MULTICAST_IF`.
#[cfg(unix)]
fn net_set_interface6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "setInterface6")?;
    let index = int_arg(args, 1);
    setsockopt_bytes(
        raw,
        libc::IPPROTO_IPV6,
        libc::IPV6_MULTICAST_IF,
        &index.to_ne_bytes(),
    )
    .map_err(|e| errno_exception(ctx, e))?;
    Ok(None)
}

/// `Net.getInterface6(fd) -> int`.
#[cfg(unix)]
fn net_get_interface6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "getInterface6")?;
    let mut buf = [0u8; 4];
    let mut len = buf.len() as libc::socklen_t;
    // SAFETY: `buf`/`len` are a valid out-pair; `raw` is kept alive by `_keep`.
    let rc = unsafe {
        libc::getsockopt(
            raw,
            libc::IPPROTO_IPV6,
            libc::IPV6_MULTICAST_IF,
            buf.as_mut_ptr().cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc != 0 {
        let e = last_errno();
        return Err(errno_exception(ctx, e));
    }
    Ok(Some(Value::Int(i32::from_ne_bytes(buf))))
}

// ---------------------------------------------------------------------------
// IOUtil pipes and the async socket's `checkConnect` (SBR wave 6, `asyncnet`)
// ---------------------------------------------------------------------------

/// libnio's `handleError` for a read/write that failed with something other
/// than "would block" / "interrupted".
#[cfg(unix)]
fn last_error_io(ctx: &mut dyn NativeContext, what: &str) -> MethodCallFailed {
    let e = last_errno();
    let text = errno_text(e);
    let _ = what;
    throw_named(ctx, "java/io/IOException", Some(&text))
}

/// `IOUtil.makePipe(boolean blocking) -> long`: `pipe(2)`; the two descriptors
/// are packed `read << 32 | write` exactly as libnio does. They are REAL OS
/// descriptors (small, outside the `net_sockets()` id range), the way the
/// selector's `EventFD` already is, so `EPoll.ctl` passes them through
/// untranslated and `FileDispatcherImpl.closeIntFD` closes them. A
/// non-blocking pipe (`blocking == false`) gets `O_NONBLOCK` on both ends.
/// The real `EPollPort` builds its wake-up pair from this.
#[cfg(unix)]
fn ioutil_make_pipe(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let blocking = int_arg(args, 0) != 0;
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` is a two-element out array, which is what `pipe` writes.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc < 0 {
        return Err(last_error_io(ctx, "Pipe failed"));
    }
    // The JDK's `PipeImpl` reads and writes these through `SocketDispatcher`
    // (JIT round 19, nio19): `net_os_pipe` serves them only once recorded.
    super::os_pipe::register(fds[0]);
    super::os_pipe::register(fds[1]);
    // SAFETY: both are descriptors `pipe` just returned; `fcntl` touches no memory.
    unsafe {
        libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        if !blocking {
            for fd in fds {
                let fl = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
            }
        }
    }
    Ok(Some(Value::Long(
        (i64::from(fds[0]) << 32) | i64::from(fds[1]),
    )))
}

/// `IOUtil.write1(int fd, byte b) -> int`: one byte to `fd`. libnio's
/// `convertReturnVal(.., reading = false)`: the count on success, 0 for a
/// zero write, `IOS_UNAVAILABLE` on `EAGAIN`, `IOS_INTERRUPTED` on `EINTR`,
/// otherwise an `IOException`.
#[cfg(unix)]
fn ioutil_write1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd = int_arg(args, 0);
    let b = int_arg(args, 1) as u8;
    // SAFETY: `&b` is one readable byte; a bad `fd` fails with -1.
    let n = unsafe { libc::write(fd, (&b as *const u8).cast::<libc::c_void>(), 1) };
    if n >= 0 {
        return Ok(Some(Value::Int(n as i32)));
    }
    match last_errno() {
        libc::EAGAIN => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
        libc::EINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
        _ => Err(last_error_io(ctx, "Write failed")),
    }
}

/// `IOUtil.drain1(int fd) -> int`: one byte from `fd`; 0 when nothing is
/// there (`EAGAIN`), `IOS_INTERRUPTED` on `EINTR`, the count otherwise.
/// A blocking pipe parks in the read, so it runs in a blocking region.
#[cfg(unix)]
fn ioutil_drain1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd = int_arg(args, 0);
    let mut b = 0u8;
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    // SAFETY: `&mut b` is one writable byte; a bad `fd` fails with -1.
    let n = unsafe { libc::read(fd, (&mut b as *mut u8).cast::<libc::c_void>(), 1) };
    let errno = if n < 0 { last_errno() } else { 0 };
    ctx.end_blocking_region();
    if n >= 0 {
        return Ok(Some(Value::Int(n as i32)));
    }
    match errno {
        libc::EAGAIN => Ok(Some(Value::Int(0))),
        libc::EINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
        // The errno captured before `end_blocking_region`, which may itself
        // make syscalls: `last_error_io` would re-read a clobbered one (round
        // nio20 wave 1, lane L5).
        e => Err(throw_named(
            ctx,
            "java/io/IOException",
            Some(&errno_text(e)),
        )),
    }
}

/// `UnixAsynchronousSocketChannelImpl.checkConnect(int fdVal)`: read
/// `SO_ERROR` off the socket an asynchronous connect was started on and throw
/// what libnio's `handleSocketError` throws for it. `fdVal` is the Java-side
/// id `Net.socket0` handed out; [`nio_os_fd`] translates it.
#[cfg(unix)]
fn unix_async_check_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let id = int_arg(args, 0);
    let keep = match nio_os_fd(id) {
        Some(k) => k,
        None => {
            return Err(throw_named(
                ctx,
                "java/io/IOException",
                Some("getsockopt: Bad file descriptor"),
            ))
        }
    };
    let mut error: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `error`/`len` are a valid out-pair; `keep` holds the socket open.
    let rc = unsafe {
        libc::getsockopt(
            keep.raw,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut error as *mut libc::c_int).cast::<libc::c_void>(),
            &mut len,
        )
    };
    if rc < 0 {
        return Err(last_error_io(ctx, "getsockopt"));
    }
    if error != 0 {
        return Err(errno_exception(ctx, error));
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Linux's answers to the three platform questions `Net.<clinit>`-adjacent
/// code asks (libnio `Net.c`: `#if defined(__linux__) return JNI_TRUE`).
#[cfg(target_os = "linux")]
const LINUX_TRUE: i32 = 1;
#[cfg(not(target_os = "linux"))]
const LINUX_TRUE: i32 = 0;

/// `shouldSetBothIPv4AndIPv6Options0`: `true` on Linux and on Windows (HotSpot 25, Windows 11, read by
/// reflection), so an `INET6` channel sets `IP_TOS` as well as `IPV6_TCLASS`.
const SET_BOTH_IP_OPTIONS: i32 = if cfg!(any(target_os = "linux", windows)) {
    1
} else {
    0
};

/// Register every native in this module, each with its kind stated. Called
/// from `register_sun_nio_ch_net`, i.e. in BOTH modes.
pub(super) fn register(r: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    let net = "sun/nio/ch/Net";
    let b = NativeKind::Bridge;

    r.register_with_kind(
        "sun/nio/ch/SocketDispatcher",
        "readv0",
        "(Ljava/io/FileDescriptor;JI)J",
        sd_readv0,
        b,
    );
    r.register_with_kind(
        "sun/nio/ch/SocketDispatcher",
        "writev0",
        "(Ljava/io/FileDescriptor;JI)J",
        sd_writev0,
        b,
    );
    r.register_with_kind(
        net,
        "shouldSetBothIPv4AndIPv6Options0",
        "()Z",
        |_c, _a| Ok(Some(Value::Int(SET_BOTH_IP_OPTIONS))),
        b,
    );
    // MEASURED on HotSpot 25.0.4 linux (reflection, and `strace` of a
    // `MulticastSocket().joinGroup(230.0.0.1)`): `false`, so an INET6 channel
    // joins an IPv4 group through `Net.join4` -> `IP_ADD_MEMBERSHIP`. `true`
    // sent it to `IPV6_ADD_MEMBERSHIP` with a v4-mapped group: EINVAL.
    r.register_with_kind(
        net,
        "canJoin6WithIPv4Group0",
        "()Z",
        |_c, _a| Ok(Some(Value::Int(0))),
        b,
    );
    r.register_with_kind(
        net,
        "canUseIPv6OptionsWithIPv4LocalAddress0",
        "()Z",
        |_c, _a| Ok(Some(Value::Int(LINUX_TRUE))),
        b,
    );
    // `NativeThread.signal0(long)`: libnio sends the "interrupt" signal to a
    // thread parked in a blocking syscall. No blocking wait in this crate needs
    // it -- every one is close-aware (sliced poll + registry re-check) -- so a
    // signal would only turn into a spurious EINTR. `signal(long)` (the Java
    // caller) is otherwise an `UnsatisfiedLinkError` once real bytecode runs it.
    r.register_with_kind(
        "sun/nio/ch/NativeThread",
        "signal0",
        "(J)V",
        |_c, _a| Ok(None),
        b,
    );

    #[cfg(windows)]
    win::register(r);

    #[cfg(unix)]
    {
        let dci = "sun/nio/ch/DatagramChannelImpl";
        r.register_with_kind(
            dci,
            "receive0",
            "(Ljava/io/FileDescriptor;JIJZ)I",
            dc_receive0,
            b,
        );
        r.register_with_kind(dci, "send0", "(Ljava/io/FileDescriptor;JIJI)I", dc_send0, b);
        r.register_with_kind(
            dci,
            "disconnect0",
            "(Ljava/io/FileDescriptor;Z)V",
            dc_disconnect0,
            b,
        );
        let dd = "sun/nio/ch/DatagramDispatcher";
        r.register_with_kind(dd, "read0", "(Ljava/io/FileDescriptor;JI)I", dd_read0, b);
        r.register_with_kind(dd, "write0", "(Ljava/io/FileDescriptor;JI)I", dd_write0, b);
        r.register_with_kind(dd, "readv0", "(Ljava/io/FileDescriptor;JI)J", dd_readv0, b);
        r.register_with_kind(
            dd,
            "writev0",
            "(Ljava/io/FileDescriptor;JI)J",
            dd_writev0,
            b,
        );
        r.register_with_kind(
            dd,
            "dup0",
            "(Ljava/io/FileDescriptor;Ljava/io/FileDescriptor;)V",
            dd_dup0,
            b,
        );
        r.register_with_kind(
            net,
            "pollConnect",
            "(Ljava/io/FileDescriptor;J)Z",
            net_poll_connect,
            b,
        );
        r.register_with_kind(
            net,
            "sendOOB",
            "(Ljava/io/FileDescriptor;B)I",
            net_send_oob,
            b,
        );
        r.register_with_kind(
            net,
            "discardOOB",
            "(Ljava/io/FileDescriptor;)Z",
            net_discard_oob,
            b,
        );
        r.register_with_kind(
            net,
            "joinOrDrop4",
            "(ZLjava/io/FileDescriptor;III)I",
            net_join_or_drop4,
            b,
        );
        r.register_with_kind(
            net,
            "blockOrUnblock4",
            "(ZLjava/io/FileDescriptor;III)I",
            net_block_or_unblock4,
            b,
        );
        r.register_with_kind(
            net,
            "joinOrDrop6",
            "(ZLjava/io/FileDescriptor;[BI[B)I",
            net_join_or_drop6,
            b,
        );
        r.register_with_kind(
            net,
            "blockOrUnblock6",
            "(ZLjava/io/FileDescriptor;[BI[B)I",
            net_block_or_unblock6,
            b,
        );
        r.register_with_kind(
            net,
            "setInterface4",
            "(Ljava/io/FileDescriptor;I)V",
            net_set_interface4,
            b,
        );
        r.register_with_kind(
            net,
            "getInterface4",
            "(Ljava/io/FileDescriptor;)I",
            net_get_interface4,
            b,
        );
        r.register_with_kind(
            net,
            "setInterface6",
            "(Ljava/io/FileDescriptor;I)V",
            net_set_interface6,
            b,
        );
        r.register_with_kind(
            net,
            "getInterface6",
            "(Ljava/io/FileDescriptor;)I",
            net_get_interface6,
            b,
        );
        // SBR wave 6 (`asyncnet`): what the real `EPollPort` and
        // `UnixAsynchronousSocketChannelImpl` end in. All genuine `ACC_NATIVE`
        // methods on 17/21/25 (`javap -p`); registered in BOTH modes.
        let iou = "sun/nio/ch/IOUtil";
        r.register_with_kind(iou, "makePipe", "(Z)J", ioutil_make_pipe, b);
        r.register_with_kind(iou, "write1", "(IB)I", ioutil_write1, b);
        r.register_with_kind(iou, "drain1", "(I)I", ioutil_drain1, b);
        r.register_with_kind(
            "sun/nio/ch/UnixAsynchronousSocketChannelImpl",
            "checkConnect",
            "(I)V",
            unix_async_check_connect,
            b,
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn ip_text_parses_every_shape_read_inet_address_text_emits() {
        assert_eq!(
            parse_ip_text("127.0.0.1"),
            Some(("127.0.0.1".parse().unwrap(), 0))
        );
        assert_eq!(parse_ip_text("::1"), Some(("::1".parse().unwrap(), 0)));
        assert_eq!(
            parse_ip_text("fe80::1%3"),
            Some(("fe80::1".parse().unwrap(), 3))
        );
        assert_eq!(parse_ip_text("not-an-ip"), None);
    }

    #[test]
    fn a_v6_socket_gets_v4_mapped_and_the_v4_wildcard_becomes_any() {
        let (sa, len) = sockaddr_for("127.0.0.1".parse().unwrap(), 0, 9, true).unwrap();
        assert_eq!(len as usize, std::mem::size_of::<libc::sockaddr_in6>());
        // SAFETY: `sockaddr_for` wrote a `sockaddr_in6` at the start of `sa`.
        let sin6 =
            unsafe { &*((&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()) };
        assert_eq!(sin6.sin6_family as i32, libc::AF_INET6);
        assert_eq!(u16::from_be(sin6.sin6_port), 9);
        assert_eq!(
            std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr),
            "::ffff:127.0.0.1".parse::<std::net::Ipv6Addr>().unwrap()
        );
        let (sa, _) = sockaddr_for("0.0.0.0".parse().unwrap(), 0, 0, true).unwrap();
        // SAFETY: as above.
        let sin6 =
            unsafe { &*((&sa as *const libc::sockaddr_storage).cast::<libc::sockaddr_in6>()) };
        assert_eq!(
            std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr),
            std::net::Ipv6Addr::UNSPECIFIED
        );
    }

    #[test]
    fn a_v4_socket_refuses_a_real_v6_address() {
        assert_eq!(
            sockaddr_for("::1".parse().unwrap(), 0, 1, false).err(),
            Some(libc::EAFNOSUPPORT)
        );
        assert!(sockaddr_for("::ffff:10.0.0.1".parse().unwrap(), 0, 1, false).is_ok());
    }

    /// An armed interruptible wait stops on the interrupt flag with the marker,
    /// an unarmed one does not look at it (a `java.net.Socket` read is not
    /// interruptible).
    #[test]
    fn an_armed_wait_stops_on_the_interrupt_and_an_unarmed_one_ignores_it() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
        let (_a, _) = l.accept().unwrap();
        let id = register_handle(NetSocketHandle::Stream(Arc::new(c.try_clone().unwrap())));
        let mut buf = [0u8; 4];
        let yes = || true;
        let e = net_read_close_aware_intr(id, &c, &mut buf, Some(&yes)).unwrap_err();
        assert!(is_nio_interrupted(&e));
        let e = net_write_close_aware_intr(id, &c, b"x", Some(&yes)).unwrap_err();
        assert!(is_nio_interrupted(&e));
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let uid = register_handle(NetSocketHandle::Datagram(Arc::new(
            udp.try_clone().unwrap(),
        )));
        let e = dgram_wait_intr(uid, &udp, NET_POLLIN, -1, Some(&yes)).unwrap_err();
        assert!(is_nio_interrupted(&e));
        // Unarmed: a bounded wait simply times out.
        assert!(!dgram_wait_intr(uid, &udp, NET_POLLIN, 30, None).unwrap());
        close_net_fd(id);
        close_net_fd(uid);
    }

    #[test]
    fn errno_text_drops_the_os_error_suffix() {
        assert_eq!(errno_text(libc::ECONNREFUSED), "Connection refused");
    }

    #[test]
    fn a_datagram_id_owns_a_real_socket_and_translates_to_it() {
        use std::os::fd::AsRawFd;
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let raw = udp.as_raw_fd();
        let id = register_handle(NetSocketHandle::Datagram(Arc::new(udp)));
        assert!(id >= 0x4000_0000);
        assert_eq!(nio_os_fd(id).map(|f| f.raw), Some(raw));
        assert!(datagram_of(id).is_some());
        close_net_fd(id);
        assert!(nio_os_fd(id).is_none());
        assert!(datagram_of(id).is_none());
    }
}
