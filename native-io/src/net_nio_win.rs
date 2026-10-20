// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Windows: the datagram, out-of-band and multicast natives the JDK's OWN `DatagramChannelImpl` /
//! `SocketChannelImpl` bytecode ends in (`net_nio_real.rs` is their Linux half, over `libc`).
//!
//! The model is the same: a `sun.nio.ch.Net` id owns a real OS socket (here a Winsock `SOCKET` created with
//! `socket()` and adopted by `std::net::UdpSocket`), blocking waits are close-aware slices of `WSAPoll`
//! (`dgram_wait_intr`), and the address blocks Java hands over are `NativeSocketAddress` memory in the
//! Winsock `SOCKADDR_IN` / `SOCKADDR_IN6` layout (`sockaddr_abi` in `lib.rs`: `AF_INET6` is 23 here), which
//! is exactly what `sendto` / `recvfrom` take, so they pass through unconverted.
//!
//! Windows differences kept from libnio's `Net.c` / `DatagramChannelImpl.c`: `SIO_UDP_CONNRESET` is switched
//! off on every datagram socket (an ICMP port-unreachable otherwise surfaces as `WSAECONNRESET` on the NEXT
//! receive), a datagram larger than the buffer is truncated rather than failed (`WSAEMSGSIZE`), and there is
//! no `MSG_DONTWAIT`: a channel in non-blocking mode has a non-blocking socket (`configureBlocking`), a
//! blocking one polls first and then reads what is ready.
//!
//! A child module of `net_nio_real` so it shares that file's private helpers.

use super::*;

use std::os::windows::io::{AsRawSocket, FromRawSocket};

type RawSock = usize;

const INVALID_SOCKET: RawSock = usize::MAX;
const SOCKET_ERROR: i32 = -1;

/// Winsock `AF_INET` / `AF_INET6` (23, not Linux's 10), and the rest of the numbers this file passes.
const AF_INET: i32 = 2;
const AF_INET6: i32 = 23;
const SOCK_DGRAM: i32 = 2;
const IPPROTO_IP: i32 = 0;
const IPPROTO_IPV6: i32 = 41;
const SOL_SOCKET: i32 = 0xffff;
const SO_REUSEADDR: i32 = 4;
const IPV6_V6ONLY: i32 = 27;
const IP_MULTICAST_IF: i32 = 9;
const IP_ADD_MEMBERSHIP: i32 = 12;
const IP_DROP_MEMBERSHIP: i32 = 13;
const IP_ADD_SOURCE_MEMBERSHIP: i32 = 15;
const IP_DROP_SOURCE_MEMBERSHIP: i32 = 16;
const IP_BLOCK_SOURCE: i32 = 17;
const IP_UNBLOCK_SOURCE: i32 = 18;
const IPV6_MULTICAST_IF: i32 = 9;
const IPV6_ADD_MEMBERSHIP: i32 = 12;
const IPV6_DROP_MEMBERSHIP: i32 = 13;
const MSG_OOB: i32 = 1;
const SIO_UDP_CONNRESET: u32 = 0x9800_000C;

const WSAEINTR: i32 = 10004;
const WSAEWOULDBLOCK: i32 = 10035;
const WSAEMSGSIZE: i32 = 10040;
const WSAENOPROTOOPT: i32 = 10042;
const WSAEOPNOTSUPP: i32 = 10045;
const WSAEAFNOSUPPORT: i32 = 10047;
const WSAECONNRESET: i32 = 10054;
const WSAECONNREFUSED: i32 = 10061;

#[link(name = "Ws2_32")]
extern "system" {
    fn WSAStartup(version: u16, data: *mut u8) -> i32;
    fn WSAGetLastError() -> i32;
    fn socket(af: i32, ty: i32, protocol: i32) -> RawSock;
    fn setsockopt(s: RawSock, level: i32, name: i32, val: *const u8, len: i32) -> i32;
    fn getsockopt(s: RawSock, level: i32, name: i32, val: *mut u8, len: *mut i32) -> i32;
    fn bind(s: RawSock, name: *const u8, len: i32) -> i32;
    fn connect(s: RawSock, name: *const u8, len: i32) -> i32;
    fn sendto(s: RawSock, buf: *const u8, len: i32, flags: i32, to: *const u8, tolen: i32) -> i32;
    fn recvfrom(
        s: RawSock,
        buf: *mut u8,
        len: i32,
        flags: i32,
        from: *mut u8,
        fromlen: *mut i32,
    ) -> i32;
    fn send(s: RawSock, buf: *const u8, len: i32, flags: i32) -> i32;
    fn recv(s: RawSock, buf: *mut u8, len: i32, flags: i32) -> i32;
    fn WSAIoctl(
        s: RawSock,
        code: u32,
        inbuf: *const u8,
        inlen: u32,
        outbuf: *mut u8,
        outlen: u32,
        returned: *mut u32,
        overlapped: *mut u8,
        completion: *mut u8,
    ) -> i32;
}

/// Winsock must be started before the first raw `socket()`; `std` does it lazily inside its own net calls,
/// which may not have run yet. `WSAStartup` is reference counted, so a second call is harmless.
fn wsa_init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let mut data = [0u8; 512];
        // SAFETY: `data` is larger than `WSADATA` on every Windows target.
        unsafe { WSAStartup(0x0202, data.as_mut_ptr()) };
    });
}

fn wsa_error() -> std::io::Error {
    // SAFETY: reads the calling thread's last Winsock error.
    std::io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn last_code() -> i32 {
    // SAFETY: as above.
    unsafe { WSAGetLastError() }
}

fn set_int_option(s: RawSock, level: i32, name: i32, val: i32) -> i32 {
    // SAFETY: the option value is a live `i32` local and its exact size is passed.
    unsafe { setsockopt(s, level, name, (&val as *const i32).cast::<u8>(), 4) }
}

fn set_bytes_option(s: RawSock, level: i32, name: i32, val: &[u8]) -> Result<(), i32> {
    // SAFETY: `val` is readable for exactly the length passed.
    let rc = unsafe { setsockopt(s, level, name, val.as_ptr(), val.len() as i32) };
    if rc == 0 {
        Ok(())
    } else {
        Err(last_code())
    }
}

/// The Winsock `sockaddr` bytes for `(ip, port)` on a socket of family `v6_socket`: an IPv4 address on an
/// IPv6 socket becomes v4-mapped (the IPv4 wildcard becomes `::`), an IPv6 address on an IPv4 socket is
/// `WSAEAFNOSUPPORT`, as libnio's `NET_InetAddressToSockaddr` does.
fn sockaddr_bytes(
    ip: std::net::IpAddr,
    scope: u32,
    port: i32,
    v6_socket: bool,
) -> Result<Vec<u8>, i32> {
    use std::net::IpAddr;
    let port = (port.clamp(0, 65535) as u16).to_be_bytes();
    if v6_socket {
        let v6 = match ip {
            IpAddr::V6(v6) => v6,
            IpAddr::V4(v4) if v4.is_unspecified() => std::net::Ipv6Addr::UNSPECIFIED,
            IpAddr::V4(v4) => v4.to_ipv6_mapped(),
        };
        let mut sa = vec![0u8; 28];
        sa[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
        sa[2..4].copy_from_slice(&port);
        sa[8..24].copy_from_slice(&v6.octets());
        sa[24..28].copy_from_slice(&scope.to_ne_bytes());
        Ok(sa)
    } else {
        let v4 = match ip {
            IpAddr::V4(v4) => v4,
            IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
                Some(v4) => v4,
                None => return Err(WSAEAFNOSUPPORT),
            },
        };
        let mut sa = vec![0u8; 16];
        sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
        sa[2..4].copy_from_slice(&port);
        sa[4..8].copy_from_slice(&v4.octets());
        Ok(sa)
    }
}

fn raw_of(udp: &std::net::UdpSocket) -> RawSock {
    udp.as_raw_socket() as RawSock
}

/// Is this datagram socket AF_INET6? `getsockname` on an UNBOUND Winsock socket fails (`WSAEINVAL`), where
/// Linux reports the family, so the family is remembered at creation, keyed by the `SOCKET` (every datagram
/// socket is made by [`net_socket0_datagram`], which overwrites a recycled handle's entry).
fn v6_sockets() -> &'static std::sync::Mutex<std::collections::HashMap<RawSock, bool>> {
    static T: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<RawSock, bool>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn udp_is_v6(udp: &std::net::UdpSocket) -> bool {
    if let Ok(map) = v6_sockets().lock() {
        if let Some(v6) = map.get(&raw_of(udp)) {
            return *v6;
        }
    }
    udp.local_addr().map(|a| a.is_ipv6()).unwrap_or(false)
}

fn sock_error(ctx: &mut dyn NativeContext, code: i32) -> MethodCallFailed {
    crate::libnio_error::handle_socket_io_error(ctx, &std::io::Error::from_raw_os_error(code))
}

// ---------------------------------------------------------------------------
// Net.socket0 / bind0 / connect0 / poll for datagrams
// ---------------------------------------------------------------------------

/// `socket0(preferIPv6, stream = false, reuse, _)`: a real UDP socket set up as libnio's `Net.socket0` does
/// on Windows: `IPV6_V6ONLY` off for a dual-stack socket, `SO_REUSEADDR` if asked, and the connection-reset
/// notification off.
pub(super) fn net_socket0_datagram(
    ctx: &mut dyn NativeContext,
    prefer_v6: bool,
    reuse: bool,
) -> Option<MethodCallResult> {
    wsa_init();
    let mut v6 = prefer_v6;
    // SAFETY: `socket` takes three integers and touches no caller memory.
    let mut s = unsafe { socket(if v6 { AF_INET6 } else { AF_INET }, SOCK_DGRAM, 0) };
    if s == INVALID_SOCKET && v6 && last_code() == WSAEAFNOSUPPORT {
        v6 = false;
        // SAFETY: as above.
        s = unsafe { socket(AF_INET, SOCK_DGRAM, 0) };
    }
    if s == INVALID_SOCKET {
        return Some(Err(sock_error(ctx, last_code())));
    }
    if v6 {
        let _ = set_int_option(s, IPPROTO_IPV6, IPV6_V6ONLY, 0);
    }
    if reuse {
        let _ = set_int_option(s, SOL_SOCKET, SO_REUSEADDR, 1);
    }
    set_connection_reset(s, false);
    if let Ok(mut map) = v6_sockets().lock() {
        map.insert(s, v6);
    }
    // SAFETY: `s` is a fresh, owned socket nothing else refers to.
    let udp = unsafe { std::net::UdpSocket::from_raw_socket(s as u64) };
    let fd = register_handle(NetSocketHandle::Datagram(Arc::new(udp)));
    dbgnet!("socket0 datagram (windows) v6={v6} reuse={reuse} -> fd={fd:#x}");
    Some(Ok(Some(Value::Int(fd))))
}

/// libnio's `setConnectionReset`: `SIO_UDP_CONNRESET`.
fn set_connection_reset(s: RawSock, enable: bool) {
    let flag: u32 = u32::from(enable);
    let mut returned = 0u32;
    // SAFETY: the in-buffer is a live `u32`, the out-pointers are valid or null as the API allows.
    unsafe {
        WSAIoctl(
            s,
            SIO_UDP_CONNRESET,
            (&flag as *const u32).cast::<u8>(),
            4,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
    }
}

pub(super) fn net_bind0_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    addr_text: &str,
    port: i32,
) -> MethodCallResult {
    let Some((ip, scope)) = parse_ip_text(addr_text) else {
        return Err(throw_named(
            ctx,
            "java/net/SocketException",
            Some("Unresolved address"),
        ));
    };
    let sa = match sockaddr_bytes(ip, scope, port, udp_is_v6(udp)) {
        Ok(v) => v,
        Err(e) => return Err(sock_error(ctx, e)),
    };
    // SAFETY: `sa` holds a valid sockaddr of the length passed; the socket is kept alive by `udp`.
    let rc = unsafe { bind(raw_of(udp), sa.as_ptr(), sa.len() as i32) };
    if rc != 0 {
        // A refused bind reads `Address already in use: bind` on HotSpot / Windows (measured, 25.0.3),
        // the same text the stream `bind0` gives; `sock_error` carried the locale's Winsock text.
        return Err(crate::net::nio_bind_err(
            ctx,
            "bind0",
            std::io::Error::from_raw_os_error(last_code()),
        ));
    }
    dbgnet!("bind0 datagram (windows) fd={fd:#x} {addr_text}:{port}");
    Ok(None)
}

pub(super) fn net_connect0_datagram(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    addr_text: &str,
    port: i32,
) -> MethodCallResult {
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
    let sa = match sockaddr_bytes(ip, scope, port, udp_is_v6(udp)) {
        Ok(v) => v,
        Err(e) => return Err(sock_error(ctx, e)),
    };
    // SAFETY: as in `net_bind0_datagram`.
    let rc = unsafe { connect(raw_of(udp), sa.as_ptr(), sa.len() as i32) };
    if rc != 0 {
        return Err(sock_error(ctx, last_code()));
    }
    // libnio: a connected datagram socket reports an ICMP error on the next call, as on Linux.
    set_connection_reset(raw_of(udp), true);
    dbgnet!("connect0 datagram (windows) fd={fd:#x} {target}");
    Ok(Some(Value::Int(1)))
}

// ---------------------------------------------------------------------------
// DatagramChannelImpl.send0 / receive0 / disconnect0
// ---------------------------------------------------------------------------

/// `receive0(fd, address, len, senderAddress, connected) -> int`.
///
/// libnio (Windows): `recvfrom` into `address`, the sender's `sockaddr` into `senderAddress`;
/// `WSAEWOULDBLOCK` -> `UNAVAILABLE`, `WSAEINTR` -> `INTERRUPTED`, `WSAEMSGSIZE` -> the datagram truncated
/// to `len`, `WSAECONNRESET` on a connected socket -> `PortUnreachableException`.
pub(super) fn dc_receive0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
                return Ok(Some(Value::Int(IOS_UNAVAILABLE)));
            }
        }
        let mut from = [0u8; 128];
        let mut from_len = from.len() as i32;
        // SAFETY: `buf` is writable for `len` bytes, `from`/`from_len` are a valid out-pair; the socket is
        // kept alive by `udp`.
        let n = unsafe {
            recvfrom(
                raw_of(&udp),
                buf.as_mut_ptr(),
                len as i32,
                0,
                from.as_mut_ptr(),
                &mut from_len,
            )
        };
        let n = if n == SOCKET_ERROR {
            match last_code() {
                WSAEWOULDBLOCK => {
                    if can_park {
                        continue;
                    }
                    return Ok(Some(Value::Int(IOS_UNAVAILABLE)));
                }
                WSAEINTR => return Ok(Some(Value::Int(IOS_INTERRUPTED))),
                // The datagram was longer than `len`: libnio keeps what fit.
                WSAEMSGSIZE => len as i32,
                WSAECONNRESET | WSAECONNREFUSED if !connected => continue,
                WSAECONNRESET | WSAECONNREFUSED => {
                    return Err(throw_named(ctx, "java/net/PortUnreachableException", None))
                }
                e => return Err(sock_error(ctx, e)),
            }
        } else {
            n
        } as usize;
        if n > 0 && !ctx.copy_to_native_memory(addr, &buf[..n]) {
            return Err(ioex(format!(
                "receive0: invalid destination address {addr:#x}"
            )));
        }
        // The whole `NativeSocketAddress` block: a v4 sender leaves the tail zero rather than whatever the
        // previous datagram's sender left.
        if !ctx.copy_to_native_memory(sender, &from[..NATIVE_SOCKADDR_LEN]) {
            return Err(ioex(format!(
                "receive0: invalid sender address {sender:#x}"
            )));
        }
        return Ok(Some(Value::Int(n as i32)));
    }
}

/// `send0(fd, address, len, targetAddress, targetAddressLen) -> int`: `sendto` with the `sockaddr`
/// `NativeSocketAddress.encode` wrote.
pub(super) fn dc_send0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd, udp) = datagram_arg(ctx, args, 0, "send0")?;
    let addr = long_arg(args, 1);
    let len = int_arg(args, 2).clamp(0, UDP_MAX_PAYLOAD as i32) as usize;
    let target = long_arg(args, 3);
    let target_len = int_arg(args, 4).clamp(0, 128) as usize;
    let mut data = vec![0u8; len];
    if len > 0 && !ctx.copy_from_native_memory(addr, &mut data) {
        return Err(ioex(format!("send0: invalid source address {addr:#x}")));
    }
    let mut to = vec![0u8; target_len];
    if target_len > 0 && !ctx.copy_from_native_memory(target, &mut to) {
        return Err(ioex(format!("send0: invalid target address {target:#x}")));
    }
    let can_park = !net_fd_is_nonblocking(fd);
    if can_park {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    // SAFETY: `data` is readable for `len` bytes, `to` holds `target_len` bytes of sockaddr, and the socket
    // is kept alive by `udp`.
    let n = unsafe {
        sendto(
            raw_of(&udp),
            data.as_ptr(),
            len as i32,
            0,
            to.as_ptr(),
            target_len as i32,
        )
    };
    let e = if n == SOCKET_ERROR { last_code() } else { 0 };
    if can_park {
        ctx.end_blocking_region();
    }
    if n == SOCKET_ERROR {
        return match e {
            WSAEWOULDBLOCK => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
            WSAEINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
            WSAECONNRESET | WSAECONNREFUSED => {
                Err(throw_named(ctx, "java/net/PortUnreachableException", None))
            }
            _ => Err(sock_error(ctx, e)),
        };
    }
    Ok(Some(Value::Int(n)))
}

/// `disconnect0(fd, isIPv6)`: `connect` with an all-zero address, as libnio does on Windows.
pub(super) fn dc_disconnect0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (_fd, udp) = datagram_arg(ctx, args, 0, "disconnect0")?;
    let sa = [0u8; 28];
    // SAFETY: `sa` is a zeroed sockaddr of the length passed; the socket is kept alive by `udp`.
    let rc = unsafe { connect(raw_of(&udp), sa.as_ptr(), sa.len() as i32) };
    if rc != 0 {
        return Err(sock_error(ctx, last_code()));
    }
    set_connection_reset(raw_of(&udp), false);
    Ok(None)
}

// ---------------------------------------------------------------------------
// DatagramDispatcher (connected datagram read/write) and dup0
// ---------------------------------------------------------------------------

/// One `recv` on a connected datagram socket (`DatagramDispatcher.read0`): `WSAECONNRESET` ->
/// `PortUnreachableException`, a truncated datagram keeps what fit.
fn dgram_recv_into(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    buf: &mut [u8],
) -> Result<i32, MethodCallFailed> {
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
        // SAFETY: `buf` is writable for its length; the socket is kept alive by `udp`.
        let n = unsafe { recv(raw_of(udp), buf.as_mut_ptr(), buf.len() as i32, 0) };
        if n == SOCKET_ERROR {
            return match last_code() {
                WSAEWOULDBLOCK if can_park => continue,
                WSAEWOULDBLOCK => Ok(IOS_UNAVAILABLE),
                WSAEINTR => Ok(IOS_INTERRUPTED),
                WSAEMSGSIZE => Ok(buf.len() as i32),
                WSAECONNRESET | WSAECONNREFUSED => {
                    Err(throw_named(ctx, "java/net/PortUnreachableException", None))
                }
                e => Err(sock_error(ctx, e)),
            };
        }
        return Ok(if n == 0 { IOS_EOF } else { n });
    }
}

/// One `send` on a connected datagram socket (`DatagramDispatcher.write0`).
fn dgram_send_from(
    ctx: &mut dyn NativeContext,
    fd: i32,
    udp: &std::net::UdpSocket,
    data: &[u8],
) -> Result<i32, MethodCallFailed> {
    let can_park = !net_fd_is_nonblocking(fd);
    if can_park {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    // SAFETY: `data` is readable for its length; the socket is kept alive by `udp`.
    let n = unsafe { send(raw_of(udp), data.as_ptr(), data.len() as i32, 0) };
    let e = if n == SOCKET_ERROR { last_code() } else { 0 };
    if can_park {
        ctx.end_blocking_region();
    }
    if n == SOCKET_ERROR {
        return match e {
            WSAEWOULDBLOCK => Ok(IOS_UNAVAILABLE),
            WSAEINTR => Ok(IOS_INTERRUPTED),
            WSAECONNRESET | WSAECONNREFUSED => {
                Err(throw_named(ctx, "java/net/PortUnreachableException", None))
            }
            _ => Err(sock_error(ctx, e)),
        };
    }
    Ok(n)
}

pub(super) fn dd_read0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(super) fn dd_write0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(super) fn dd_readv0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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

pub(super) fn dd_writev0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd, udp) = datagram_arg(ctx, args, 0, "writev0")?;
    let iov = read_iovecs(ctx, long_arg(args, 1), int_arg(args, 2))?;
    let data = gather(ctx, &iov, UDP_MAX_PAYLOAD)?;
    let n = dgram_send_from(ctx, fd, &udp, &data)?;
    Ok(Some(Value::Long(i64::from(n))))
}

/// `DatagramDispatcher.dup0(fd1, fd2)`: `fd2` thereafter names a duplicate of `fd1`'s socket.
pub(super) fn dd_dup0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (fd1, udp1) = datagram_arg(ctx, args, 0, "dup0")?;
    let fd2_obj = obj_arg(args, 1)?;
    let fd2 = net_fd_from_descriptor(ctx, fd2_obj)
        .ok_or_else(|| ioex("dup0: FileDescriptor has no fd id"))?;
    let dup = udp1
        .try_clone()
        .map_err(|e| crate::libnio_error::io_exception_with_last_error(&e, "dup failed"))?;
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
// Net.pollConnect / sendOOB / discardOOB
// ---------------------------------------------------------------------------

/// `Net.pollConnect(fd, timeout) -> boolean` (see the Linux half): the connection is complete when the JDK
/// asks, so poll for writability and read `SO_ERROR`.
pub(super) fn net_poll_connect(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("pollConnect: FileDescriptor has no fd id"))?;
    // The failure the non-blocking connect's probe already saw (`net_deferred_connect_failures`).
    let deferred = net_deferred_connect_failures().lock().remove(&fd);
    if let Some(e) = deferred {
        let base = if e.kind() == std::io::ErrorKind::ConnectionRefused {
            "Connection refused".to_string()
        } else {
            e.raw_os_error()
                .map(crate::libnio_error::strerror)
                .unwrap_or_else(|| e.to_string())
        };
        return Err(crate::libnio_error::throw_by_name(
            ctx,
            crate::libnio_error::socket_error_class_of_kind(e.kind()),
            Some(&format!("{base}: getsockopt")),
        ));
    }
    let stream = match net_sockets().read().get(&fd) {
        Some(NetSocketHandle::Stream(s)) => Arc::clone(s),
        _ => return Ok(Some(Value::Int(0))),
    };
    let timeout = long_arg(args, 1).clamp(-1, i64::from(i32::MAX)) as i32;
    if timeout != 0 {
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    }
    let ready = net_poll_raw(stream.as_raw_socket() as NetRawHandle, NET_POLLOUT, timeout);
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
            // Windows words a failed pending connect `<text>: getsockopt` (`Net.pollConnect`).
            let code = e.raw_os_error().unwrap_or(WSAECONNREFUSED);
            let base = if code == WSAECONNREFUSED {
                "Connection refused".to_string()
            } else {
                crate::libnio_error::strerror(code)
            };
            let text = format!("{base}: getsockopt");
            Err(crate::libnio_error::throw_by_name(
                ctx,
                crate::libnio_error::socket_error_class_of_kind(e.kind()),
                Some(&text),
            ))
        }
        Err(e) => Err(nio_net_err(ctx, "pollConnect", e)),
    }
}

/// `Net.sendOOB(fd, byte) -> int`: `send(.., MSG_OOB)`.
pub(super) fn net_send_oob(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    let n = unsafe { send(stream.as_raw_socket() as RawSock, b.as_ptr(), 1, MSG_OOB) };
    if n == SOCKET_ERROR {
        return match last_code() {
            WSAEWOULDBLOCK => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
            WSAEINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
            e => Err(sock_error(ctx, e)),
        };
    }
    Ok(Some(Value::Int(n)))
}

/// `Net.discardOOB(fd) -> boolean`: drain pending urgent data. The socket is polled for `POLLRDBAND` first
/// (a `recv(MSG_OOB)` with none pending blocks on a blocking socket).
pub(super) fn net_discard_oob(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let fd_obj = obj_arg(args, 0)?;
    let fd = net_fd_from_descriptor(ctx, fd_obj)
        .ok_or_else(|| ioex("discardOOB: FileDescriptor has no fd id"))?;
    let stream = match net_sockets().read().get(&fd) {
        Some(NetSocketHandle::Stream(s)) => Arc::clone(s),
        _ => return Ok(Some(Value::Int(0))),
    };
    const NET_POLLRDBAND: i32 = 0x0200;
    let raw = stream.as_raw_socket() as RawSock;
    let mut discarded = false;
    let mut buf = [0u8; 8];
    while matches!(
        net_poll_raw(raw as NetRawHandle, NET_POLLRDBAND, 0),
        Ok(true)
    ) {
        // SAFETY: `buf` is a writable local; the socket is kept alive by `stream`.
        let n = unsafe { recv(raw, buf.as_mut_ptr(), buf.len() as i32, MSG_OOB) };
        if n > 0 {
            discarded = true;
            continue;
        }
        break;
    }
    Ok(Some(Value::Int(i32::from(discarded))))
}

// ---------------------------------------------------------------------------
// Multicast membership and interface
// ---------------------------------------------------------------------------

fn dgram_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    idx: usize,
    what: &str,
) -> Result<(RawSock, Arc<std::net::UdpSocket>), MethodCallFailed> {
    let (_fd, udp) = datagram_arg(ctx, args, idx, what)?;
    Ok((raw_of(&udp), udp))
}

fn unsupported_option(code: i32) -> bool {
    code == WSAENOPROTOOPT || code == WSAEOPNOTSUPP
}

/// `Net.joinOrDrop4(join, fd, group, interf, source) -> int`: `IP_ADD_MEMBERSHIP` / `IP_DROP_MEMBERSHIP`
/// (`struct ip_mreq`), or with a source `IP_ADD_SOURCE_MEMBERSHIP` / `IP_DROP_SOURCE_MEMBERSHIP`
/// (`struct ip_mreq_source`: group, source, interface on Windows).
pub(super) fn net_join_or_drop4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let join = bool_arg(args, 0);
    let (raw, _keep) = dgram_raw(ctx, args, 1, "joinOrDrop4")?;
    let group = (int_arg(args, 2) as u32).to_be_bytes();
    let interf = (int_arg(args, 3) as u32).to_be_bytes();
    let source = int_arg(args, 4);
    let r = if source == 0 {
        let mut mreq = [0u8; 8];
        mreq[0..4].copy_from_slice(&group);
        mreq[4..8].copy_from_slice(&interf);
        set_bytes_option(
            raw,
            IPPROTO_IP,
            if join {
                IP_ADD_MEMBERSHIP
            } else {
                IP_DROP_MEMBERSHIP
            },
            &mreq,
        )
    } else {
        let mut mreq = [0u8; 12];
        mreq[0..4].copy_from_slice(&group);
        mreq[4..8].copy_from_slice(&(source as u32).to_be_bytes());
        mreq[8..12].copy_from_slice(&interf);
        set_bytes_option(
            raw,
            IPPROTO_IP,
            if join {
                IP_ADD_SOURCE_MEMBERSHIP
            } else {
                IP_DROP_SOURCE_MEMBERSHIP
            },
            &mreq,
        )
    };
    match r {
        Ok(()) => Ok(Some(Value::Int(0))),
        Err(e) if join && unsupported_option(e) => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
        Err(e) => Err(sock_error(ctx, e)),
    }
}

/// `Net.blockOrUnblock4(block, fd, group, interf, source) -> int`.
pub(super) fn net_block_or_unblock4(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let block = bool_arg(args, 0);
    let (raw, _keep) = dgram_raw(ctx, args, 1, "blockOrUnblock4")?;
    let mut mreq = [0u8; 12];
    mreq[0..4].copy_from_slice(&(int_arg(args, 2) as u32).to_be_bytes());
    mreq[4..8].copy_from_slice(&(int_arg(args, 4) as u32).to_be_bytes());
    mreq[8..12].copy_from_slice(&(int_arg(args, 3) as u32).to_be_bytes());
    match set_bytes_option(
        raw,
        IPPROTO_IP,
        if block {
            IP_BLOCK_SOURCE
        } else {
            IP_UNBLOCK_SOURCE
        },
        &mreq,
    ) {
        Ok(()) => Ok(Some(Value::Int(0))),
        Err(e) if block && unsupported_option(e) => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
        Err(e) => Err(sock_error(ctx, e)),
    }
}

/// `Net.joinOrDrop6(join, fd, group, index, source) -> int`: `IPV6_ADD_MEMBERSHIP` / `IPV6_DROP_MEMBERSHIP`
/// (`struct ipv6_mreq`); a source-specific join answers `UNAVAILABLE`.
pub(super) fn net_join_or_drop6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
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
    match set_bytes_option(
        raw,
        IPPROTO_IPV6,
        if join {
            IPV6_ADD_MEMBERSHIP
        } else {
            IPV6_DROP_MEMBERSHIP
        },
        &mreq,
    ) {
        Ok(()) => Ok(Some(Value::Int(0))),
        Err(e) if join && unsupported_option(e) => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
        Err(e) => Err(sock_error(ctx, e)),
    }
}

/// `Net.blockOrUnblock6(..) -> int`: source filtering on IPv6 is not modelled; `UNAVAILABLE` is the JDK's
/// own "not supported" answer.
pub(super) fn net_block_or_unblock6(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let _ = dgram_raw(ctx, args, 1, "blockOrUnblock6")?;
    Ok(Some(Value::Int(IOS_UNAVAILABLE)))
}

pub(super) fn net_set_interface4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "setInterface4")?;
    let addr = (int_arg(args, 1) as u32).to_be_bytes();
    set_bytes_option(raw, IPPROTO_IP, IP_MULTICAST_IF, &addr).map_err(|e| sock_error(ctx, e))?;
    Ok(None)
}

fn get_int_option(raw: RawSock, level: i32, name: i32) -> Result<[u8; 4], i32> {
    let mut buf = [0u8; 4];
    let mut len = 4i32;
    // SAFETY: `buf`/`len` are a valid out-pair; `raw` is kept alive by the caller.
    let rc = unsafe { getsockopt(raw, level, name, buf.as_mut_ptr(), &mut len) };
    if rc != 0 {
        Err(last_code())
    } else {
        Ok(buf)
    }
}

pub(super) fn net_get_interface4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "getInterface4")?;
    let buf = get_int_option(raw, IPPROTO_IP, IP_MULTICAST_IF).map_err(|e| sock_error(ctx, e))?;
    Ok(Some(Value::Int(u32::from_be_bytes(buf) as i32)))
}

pub(super) fn net_set_interface6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "setInterface6")?;
    let index = int_arg(args, 1);
    set_bytes_option(raw, IPPROTO_IPV6, IPV6_MULTICAST_IF, &index.to_ne_bytes())
        .map_err(|e| sock_error(ctx, e))?;
    Ok(None)
}

pub(super) fn net_get_interface6(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let (raw, _keep) = dgram_raw(ctx, args, 0, "getInterface6")?;
    let buf =
        get_int_option(raw, IPPROTO_IPV6, IPV6_MULTICAST_IF).map_err(|e| sock_error(ctx, e))?;
    Ok(Some(Value::Int(i32::from_ne_bytes(buf))))
}

// ---------------------------------------------------------------------------
// IOUtil.write1 / drain / drain1 (the selector's wakeup pair)
// ---------------------------------------------------------------------------

/// `IOUtil.write1(fd, b) -> int`: one byte to the socket behind `fd` (the Windows selector's wakeup pair is a
/// socket pair; libnio's Windows `write1` is `send`).
pub(super) fn ioutil_write1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(os) = crate::net::nio_os_fd(int_arg(args, 0)) else {
        return Err(throw_named(
            ctx,
            "java/io/IOException",
            Some("Bad file descriptor"),
        ));
    };
    let b = [int_arg(args, 1) as u8];
    // SAFETY: `b` is a one-byte local; `os` keeps the socket open across the call.
    let n = unsafe { send(os.raw, b.as_ptr(), 1, 0) };
    if n >= 0 {
        return Ok(Some(Value::Int(n)));
    }
    match last_code() {
        WSAEWOULDBLOCK => Ok(Some(Value::Int(IOS_UNAVAILABLE))),
        WSAEINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
        e => Err(sock_error(ctx, e)),
    }
}

/// `IOUtil.drain(fd) -> boolean`: read what is pending, 128 bytes at a time while a read fills the buffer;
/// true when anything was read. A read is only issued while the socket polls readable, so a blocking socket
/// is never parked on.
pub(crate) fn ioutil_drain(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(os) = crate::net::nio_os_fd(int_arg(args, 0)) else {
        return Ok(Some(Value::Int(0)));
    };
    let mut drained = false;
    let mut buf = [0u8; 128];
    while matches!(
        net_poll_raw(os.raw as NetRawHandle, NET_POLLIN, 0),
        Ok(true)
    ) {
        // SAFETY: `buf` is a writable local; `os` keeps the socket open across the call.
        let n = unsafe { recv(os.raw, buf.as_mut_ptr(), buf.len() as i32, 0) };
        if n > 0 {
            drained = true;
            if n as usize == buf.len() {
                continue;
            }
        }
        break;
    }
    Ok(Some(Value::Int(i32::from(drained))))
}

/// `IOUtil.drain1(fd) -> int`: one byte if one is pending, 0 otherwise.
pub(super) fn ioutil_drain1(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(os) = crate::net::nio_os_fd(int_arg(args, 0)) else {
        return Err(throw_named(
            ctx,
            "java/io/IOException",
            Some("Bad file descriptor"),
        ));
    };
    if !matches!(
        net_poll_raw(os.raw as NetRawHandle, NET_POLLIN, 0),
        Ok(true)
    ) {
        return Ok(Some(Value::Int(0)));
    }
    let mut b = 0u8;
    // SAFETY: `b` is a one-byte local; `os` keeps the socket open across the call.
    let n = unsafe { recv(os.raw, &mut b, 1, 0) };
    if n >= 0 {
        return Ok(Some(Value::Int(n)));
    }
    match last_code() {
        WSAEWOULDBLOCK => Ok(Some(Value::Int(0))),
        WSAEINTR => Ok(Some(Value::Int(IOS_INTERRUPTED))),
        e => Err(sock_error(ctx, e)),
    }
}

/// Registration of the natives only this file provides (the `cfg(unix)` block of `register` is its Linux
/// twin). Each is a genuine `ACC_NATIVE` method on the Windows JDK image.
pub(super) fn register(r: &mut NativeMethodRegistry) {
    use cratonvm_native_api::NativeKind;
    let b = NativeKind::Bridge;
    let net = "sun/nio/ch/Net";
    let dci = "sun/nio/ch/DatagramChannelImpl";
    let iou = "sun/nio/ch/IOUtil";
    r.register_with_kind(iou, "write1", "(IB)I", ioutil_write1, b);
    r.register_with_kind(iou, "drain1", "(I)I", ioutil_drain1, b);
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_v4_address_on_a_v6_socket_is_v4_mapped_and_the_wildcard_is_any() {
        let sa = sockaddr_bytes("127.0.0.1".parse().unwrap(), 0, 9, true).unwrap();
        assert_eq!(sa.len(), 28);
        assert_eq!(u16::from_ne_bytes([sa[0], sa[1]]), AF_INET6 as u16);
        assert_eq!(u16::from_be_bytes([sa[2], sa[3]]), 9);
        assert_eq!(
            &sa[8..24],
            &"::ffff:127.0.0.1"
                .parse::<std::net::Ipv6Addr>()
                .unwrap()
                .octets()
        );
        let any = sockaddr_bytes("0.0.0.0".parse().unwrap(), 0, 0, true).unwrap();
        assert_eq!(&any[8..24], &[0u8; 16]);
    }

    #[test]
    fn a_v4_socket_refuses_a_real_v6_address_and_takes_a_mapped_one() {
        assert_eq!(
            sockaddr_bytes("::1".parse().unwrap(), 0, 1, false).err(),
            Some(WSAEAFNOSUPPORT)
        );
        let sa = sockaddr_bytes("::ffff:10.0.0.1".parse().unwrap(), 0, 1, false).unwrap();
        assert_eq!(sa.len(), 16);
        assert_eq!(u16::from_ne_bytes([sa[0], sa[1]]), AF_INET as u16);
        assert_eq!(&sa[4..8], &[10, 0, 0, 1]);
    }

    /// An unbound dual-stack datagram socket is AF_INET6 although `getsockname` refuses it on Winsock, and
    /// binds to a v4 loopback address (v4-mapped) like HotSpot's does.
    #[test]
    fn an_unbound_datagram_socket_remembers_its_family_and_binds_to_loopback() {
        let mut ctx = crate::test_support::MockNativeContext::new();
        let fd = match net_socket0_datagram(&mut ctx, true, false) {
            Some(Ok(Some(Value::Int(fd)))) => fd,
            other => panic!("socket0 failed: {other:?}"),
        };
        let udp = datagram_of(fd).expect("registered");
        assert!(udp_is_v6(&udp));
        net_bind0_datagram(&mut ctx, fd, &udp, "127.0.0.1", 0).expect("bind");
        assert_ne!(udp.local_addr().unwrap().port(), 0);
        close_net_fd(fd);
    }
}
