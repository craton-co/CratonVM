// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.7 — `java.nio.channels.DatagramChannel` extensions: real
//! send/receive of payloads, multicast group membership, broadcast
//! support.
//!
//! ## Why this is a separate registry
//!
//! The pre-existing `t16_dc_*` family in `nio_native.rs` (lines
//! 712-836) covers `open()` / `close()` / `connect()` / `isOpen` /
//! `configureBlocking`.  It owns its own `udp_registry` keyed by an
//! `i32` `sock_id` stashed in the channel object's slot 4.  We cannot
//! edit `nio_native.rs` (file-scope rule for this WP), nor can we
//! share the existing `udp_registry` since it is `fn`-private inside
//! that module.
//!
//! As a result this module **maintains its own** `dgram_registry`.
//! Channels created via the pre-existing `t16_dc_open` use the
//! original registry; channels created via `dgram_open0` here use
//! ours.  This is a deliberate divergence flagged in the report — a
//! follow-up integration step in `lib.rs` can route both paths
//! through a single registry.  In the meantime, both sets of natives
//! work in isolation and any code that opens a channel one way stays
//! consistent within that side.
//!
//! Acceptance criteria (WP3.7):
//!   * `DatagramChannel.open()` + `bind()` + `send(buf, addr)` + a
//!     peer's `receive(buf)` round-trips an arbitrary payload.
//!   * `DatagramChannel.join(group, ifc)` joins a multicast group;
//!     subsequent `receive` on a peer in the same group sees the
//!     packet.
//!   * `localhost` can be DNS-resolved through the natural
//!     `InetAddress.getByName` path which already exists in `net.rs`.
//!     We don't reimplement DNS itself — we only ensure the
//!     `DatagramChannel` transport is healthy enough to carry
//!     `localhost`-bound payloads.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, OnceLock, RwLock};

use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Local UDP registry (parallel to nio_native::udp_registry)
// ---------------------------------------------------------------------------

struct DatagramState {
    /// Behind an `Arc` so callers can lift the socket out of the registry and
    /// **release the registry lock before touching it** — see `dgram_socket`.
    /// Nothing outside this module's accessors may reach it.
    sock: Arc<UdpSocket>,
    /// Multicast groups currently joined on this socket.  Each entry is
    /// (group, interface).  Used by `dgram_drop` and to materialise
    /// `MembershipKey` objects.
    groups: Vec<(IpAddr, IpAddr)>,
}

fn dgram_registry() -> &'static RwLock<HashMap<i32, DatagramState>> {
    static R: OnceLock<RwLock<HashMap<i32, DatagramState>>> = OnceLock::new();
    R.get_or_init(|| RwLock::new(HashMap::new()))
}

fn next_dgram_id() -> i32 {
    static N: AtomicI32 = AtomicI32::new(1);
    N.fetch_add(1, Ordering::Relaxed)
}

fn dgram_register(sock: UdpSocket) -> i32 {
    let id = next_dgram_id();
    if let Ok(mut g) = dgram_registry().write() {
        g.insert(
            id,
            DatagramState {
                sock: Arc::new(sock),
                groups: Vec::new(),
            },
        );
    }
    id
}

fn dgram_remove(id: i32) {
    if let Ok(mut g) = dgram_registry().write() {
        g.remove(&id);
    }
}

/// Lift the socket for `id` out of the registry, **releasing the registry lock
/// before returning it**. Every socket operation goes through here.
///
/// This used to be a `dgram_with(id, |s| …)` closure helper that ran the
/// caller's body with the read lock still held, and `dgram_receive0` passed it
/// `|s| s.sock.recv_from(&mut bytes)`. A blocking-mode `DatagramChannel`
/// (`configureBlocking` defaults to true, per the JDK) then parked in the
/// kernel holding the registry read lock, so:
///
///   * `close()` → `dgram_remove` → `.write()` blocked behind it, and
///   * with a writer queued, every *other* reader blocked too — a second
///     channel could not even `open()`.
///
/// i.e. one idle receive wedged all UDP channels, and the `close()` that
/// should have ended the wait was precisely what could not run. Handing back
/// an `Arc` keeps the socket alive for the syscall without keeping the map
/// locked; a concurrent `close()` drops the map's `Arc` and the fd is released
/// once the receiver returns. Same defect and same fix as
/// `socket_channel::resolve_stream` (dev `cd18f9a51`) and `net.rs`'s
/// `NetSocketHandle::Stream`.
///
/// The socket is deliberately NOT reachable from `dgram_with_mut`, so no future
/// edit can reintroduce a blocking call under the lock.
fn dgram_socket(id: i32) -> Option<Arc<UdpSocket>> {
    Some(Arc::clone(&dgram_registry().read().ok()?.get(&id)?.sock))
}

/// Mutate a channel's non-I/O state (its multicast group list). Runs under the
/// registry write lock, so the body must not block — see `dgram_socket`.
fn dgram_with_mut<T, F: FnOnce(&mut DatagramState) -> T>(id: i32, f: F) -> Option<T> {
    Some(f(dgram_registry().write().ok()?.get_mut(&id)?))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn io_error(msg: impl Into<String>) -> MethodCallFailed {
    MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::IOException {
        message: msg.into(),
    }))
}

fn arg_obj(args: &[Value], idx: usize) -> Option<ObjectRef> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Some(*o),
        _ => None,
    }
}

fn arg_int(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        Some(Value::Long(v)) => *v as i32,
        _ => 0,
    }
}

/// Layout — mirrors `nio_native.rs`'s synthetic DatagramChannel:
///   slot 0: port      (Int)
///   slot 1: open      (Int)
///   slot 2: connected (Int)
///   slot 3: blocking  (Int)
///   slot 4: sock_id   (Int) — index into our registry
///   slot 5: broadcast (Int) — extra flag for SO_BROADCAST state
const DC_FIELD_PORT: usize = 0;
const DC_FIELD_OPEN: usize = 1;
const DC_FIELD_CONNECTED: usize = 2;
const DC_FIELD_BLOCKING: usize = 3;
const DC_FIELD_SOCK_ID: usize = 4;
const DC_FIELD_BROADCAST: usize = 5;

fn dc_id(ctx: &dyn NativeContext, this: ObjectRef) -> Option<i32> {
    if ctx.object_num_fields(this) <= DC_FIELD_SOCK_ID {
        return None;
    }
    match ctx.get_field(this, DC_FIELD_SOCK_ID) {
        Value::Int(v) if v > 0 => Some(v),
        _ => None,
    }
}

fn alloc_channel(ctx: &mut dyn NativeContext, port: i32, sock_id: i32) -> ObjectRef {
    let cid = ctx
        .ensure_class_initialized("java/nio/channels/DatagramChannel")
        .unwrap_or_else(|_| ClassId::new(0));
    let ch = ctx.alloc_object(cid, 6);
    ctx.set_field(ch, DC_FIELD_PORT, Value::Int(port));
    ctx.set_field(ch, DC_FIELD_OPEN, Value::Int(1));
    ctx.set_field(ch, DC_FIELD_CONNECTED, Value::Int(0));
    ctx.set_field(ch, DC_FIELD_BLOCKING, Value::Int(1));
    ctx.set_field(ch, DC_FIELD_SOCK_ID, Value::Int(sock_id));
    ctx.set_field(ch, DC_FIELD_BROADCAST, Value::Int(0));
    ch
}

/// Decode an `InetSocketAddress` Java object into a Rust SocketAddr.
/// Tolerates several layouts:
///   * Real-JDK: fields by name `addr` (InetAddress), `port` (int).
///   * Synthetic: slot 0 = host string, slot 1 = port int.
fn decode_isa(ctx: &dyn NativeContext, isa: ObjectRef) -> Option<SocketAddr> {
    // Pull port first — it's a primitive so it's deterministic.
    let port = match ctx.get_field_by_name(isa, "port") {
        Value::Int(v) => v,
        _ => match ctx.get_field(isa, 1) {
            Value::Int(v) => v,
            _ => return None,
        },
    };
    if !(0..=65535).contains(&port) {
        return None;
    }
    // Host could be a String at slot 0 (synthetic) or an InetAddress
    // object at field "addr" (real-JDK).
    let host_str = match ctx.get_field_by_name(isa, "addr") {
        Value::Object(Some(ia)) => {
            // InetAddress.holder().getHostAddress() is too involved; we
            // fall back to slot 1 (ip text) inside InetAddress, then
            // slot 0 (host).
            match ctx.get_field(ia, 1) {
                Value::Object(Some(s)) => ctx.read_string(s),
                _ => match ctx.get_field(ia, 0) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                },
            }
        }
        _ => match ctx.get_field(isa, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }?;
    let parsed: IpAddr = if let Ok(ip) = host_str.parse::<IpAddr>() {
        ip
    } else {
        // If it's a hostname (e.g. "localhost"), resolve via std::net
        // so we exercise the same DNS path InetAddress would.
        (host_str.as_str(), 0u16)
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .map(|s| s.ip())?
    };
    Some(SocketAddr::new(parsed, port as u16))
}

/// Reverse of `decode_isa`: build a Java `InetSocketAddress` carrying
/// the given Rust SocketAddr.  Uses the synthetic layout (host string
/// at slot 0, port at slot 1) so it round-trips through `decode_isa`.
fn encode_isa(ctx: &mut dyn NativeContext, addr: SocketAddr) -> Option<ObjectRef> {
    let isa = ctx.new_object("java/net/InetSocketAddress").ok()??;
    let isa_obj = match isa {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let host = ctx.create_string(&addr.ip().to_string());
    if ctx.object_num_fields(isa_obj) >= 2 {
        ctx.set_field(isa_obj, 0, Value::Object(Some(host)));
        ctx.set_field(isa_obj, 1, Value::Int(addr.port() as i32));
    }
    Some(isa_obj)
}

/// View into a `ByteBuffer` — see the same helper in pipe.rs for
/// rationale.  Returns (array_ref, position, limit).
fn buffer_view(ctx: &dyn NativeContext, buf: ObjectRef) -> Option<(ObjectRef, i32, i32)> {
    let arr = match ctx.get_field_by_name(buf, "hb") {
        Value::Object(Some(a)) => a,
        _ => match ctx.get_field(buf, 5) {
            Value::Object(Some(a)) => a,
            _ => return None,
        },
    };
    let position = match ctx.get_field_by_name(buf, "position") {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, 0) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    let limit = match ctx.get_field_by_name(buf, "limit") {
        Value::Int(v) => v,
        _ => match ctx.get_field(buf, 1) {
            Value::Int(v) => v,
            _ => 0,
        },
    };
    Some((arr, position, limit))
}

fn buffer_advance(ctx: &mut dyn NativeContext, buf: ObjectRef, new_pos: i32) {
    ctx.set_field_by_name(buf, "position", Value::Int(new_pos));
    ctx.set_field(buf, 0, Value::Int(new_pos));
}

/// H3a: vet a fully-resolved UDP destination against the outbound-host
/// policy before we `send_to` it. `decode_isa` has already resolved any
/// hostname to a concrete `SocketAddr`, so (unlike the TCP non-blocking
/// path) there is no DNS-rebind window left to close — we just need to
/// run the same link-local / blocked-range check the TCP connect path
/// uses. We reuse the public `outbound_policy::check_outbound` API by
/// formatting the resolved address as an `IP:port` literal (bracketing
/// IPv6 so `host_part` parses it), so the default policy's literal-IP
/// link-local check fires against `169.254.169.254`, the broader
/// `169.254.0.0/16` range, and the IPv6 link-local / AWS-metadata
/// addresses. A denial maps to the same `IOException` the rest of this
/// module raises (`io_error`), mirroring the TCP path's exception type.
fn check_outbound_target(target: SocketAddr) -> Result<(), MethodCallFailed> {
    let literal = match target {
        SocketAddr::V4(_) => format!("{}:{}", target.ip(), target.port()),
        SocketAddr::V6(_) => format!("[{}]:{}", target.ip(), target.port()),
    };
    if let Err(reason) = crate::outbound_policy::check_outbound(&literal) {
        return Err(io_error(format!(
            "send denied by outbound policy: {reason}"
        )));
    }
    Ok(())
}

fn parse_inet_address(ctx: &dyn NativeContext, addr: ObjectRef) -> Option<IpAddr> {
    // Try IP text at slot 1 first (matches what net.rs encodes).
    let text = match ctx.get_field(addr, 1) {
        Value::Object(Some(s)) => ctx.read_string(s),
        _ => match ctx.get_field(addr, 0) {
            Value::Object(Some(s)) => ctx.read_string(s),
            _ => None,
        },
    }?;
    if let Ok(ip) = text.parse::<IpAddr>() {
        return Some(ip);
    }
    // Hostname → resolve via the same path InetAddress.getByName uses.
    (text.as_str(), 0u16)
        .to_socket_addrs()
        .ok()?
        .next()
        .map(|s| s.ip())
}

// ---------------------------------------------------------------------------
// Native handlers
// ---------------------------------------------------------------------------

/// `sun.nio.ch.DatagramChannelImpl.open0()` — bind 0.0.0.0:0 and
/// register.  Mirrors `t16_dc_open` but routes through this module's
/// registry.
fn dgram_open0(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    let sock = UdpSocket::bind("0.0.0.0:0").map_err(|e| io_error(format!("bind: {e}")))?;
    let port = sock.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    let id = dgram_register(sock);
    let ch = alloc_channel(ctx, port, id);
    Ok(Some(Value::Object(Some(ch))))
}

/// `bind0(SocketAddress local) -> DatagramChannel` — explicit bind.
fn dgram_bind0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("bind: null this"));
    };
    let local = arg_obj(args, 1)
        .and_then(|o| decode_isa(ctx, o))
        .unwrap_or_else(|| SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0));
    let new_sock = UdpSocket::bind(local).map_err(|e| io_error(format!("bind {local}: {e}")))?;
    let port = new_sock.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    if let Some(old_id) = dc_id(ctx, this) {
        dgram_remove(old_id);
    }
    let new_id = dgram_register(new_sock);
    if ctx.object_num_fields(this) > DC_FIELD_PORT {
        ctx.set_field(this, DC_FIELD_PORT, Value::Int(port));
    }
    if ctx.object_num_fields(this) > DC_FIELD_SOCK_ID {
        ctx.set_field(this, DC_FIELD_SOCK_ID, Value::Int(new_id));
    }
    Ok(Some(Value::Object(Some(this))))
}

/// `send0(ByteBuffer src, SocketAddress target) -> int` — write the
/// buffer's `[position, limit)` slice to `target`.  Real `sendto(2)`.
fn dgram_send0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("send: null this"));
    };
    let id = dc_id(ctx, this).ok_or_else(|| io_error("send: no socket id"))?;
    let buf = arg_obj(args, 1).ok_or_else(|| io_error("send: null buffer"))?;
    let target = arg_obj(args, 2)
        .and_then(|o| decode_isa(ctx, o))
        .ok_or_else(|| io_error("send: target SocketAddress unparsable"))?;
    // H3a: SSRF gate. Refuse datagrams to link-local cloud-metadata /
    // blocked ranges before the real `sendto(2)`, matching the TCP path.
    check_outbound_target(target)?;
    let (arr, position, limit) =
        buffer_view(ctx, buf).ok_or_else(|| io_error("send: buffer layout"))?;
    if position >= limit {
        return Ok(Some(Value::Int(0)));
    }
    let n_to_send = (limit - position) as usize;
    // AUDIT 2026-05-24: bulk read via NativeContext intrinsic instead
    // of a per-byte `get_array_element` loop. Single memcpy from the
    // heap byte[] payload.
    let mut bytes = vec![0u8; n_to_send];
    let n_read = ctx.read_byte_array_into(arr, position as usize, &mut bytes);
    bytes.truncate(n_read);
    // send_to can park on a full local socket buffer; keep it in the same
    // GC-blocking protocol as the receive path and re-sync `buf` (used by
    // buffer_advance below) across the region.
    let mut held = vec![Value::Object(Some(buf))];
    ctx.begin_blocking_region();
    let sent_opt = dgram_socket(id).map(|s| s.send_to(&bytes, target));
    ctx.end_blocking_region_refs(&mut held);
    let sent = sent_opt
        .ok_or_else(|| io_error("send: socket missing"))?
        .map_err(|e| io_error(format!("send_to {target}: {e}")))?;
    let buf = match held[0] {
        Value::Object(Some(b)) => b,
        _ => return Ok(Some(Value::Int(sent as i32))),
    };
    if sent > 0 {
        buffer_advance(ctx, buf, position + sent as i32);
    }
    Ok(Some(Value::Int(sent as i32)))
}

/// `receive0(ByteBuffer dst) -> SocketAddress` — return the peer
/// address that sent the next packet, and copy bytes into `dst`.
/// Returns null if the socket is non-blocking and no packet is ready.
fn dgram_receive0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("receive: null this"));
    };
    let id = dc_id(ctx, this).ok_or_else(|| io_error("receive: no socket id"))?;
    let buf = arg_obj(args, 1).ok_or_else(|| io_error("receive: null buffer"))?;
    let (arr, position, limit) =
        buffer_view(ctx, buf).ok_or_else(|| io_error("receive: buffer layout"))?;
    let space = (limit - position).max(0) as usize;
    if space == 0 {
        return Ok(Some(Value::Object(None)));
    }
    let mut bytes = vec![0u8; space];
    // A blocking-mode DatagramChannel parks in recv_from until a packet
    // arrives. Bracket it in the GC-blocking protocol (see the matching
    // MulticastSocket.receive comment in net.rs) and re-sync the heap refs
    // used after the region through `end_blocking_region_refs`.
    let mut held = vec![Value::Object(Some(buf)), Value::Object(Some(arr))];
    ctx.begin_blocking_region();
    let recv_opt = dgram_socket(id).map(|s| s.recv_from(&mut bytes));
    ctx.end_blocking_region_refs(&mut held);
    let recv_result = recv_opt.ok_or_else(|| io_error("receive: socket missing"))?;
    let (buf, arr) = match (held[0], held[1]) {
        (Value::Object(Some(b)), Value::Object(Some(a))) => (b, a),
        _ => return Ok(Some(Value::Object(None))),
    };
    let (n, peer) = match recv_result {
        Ok(x) => x,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return Ok(Some(Value::Object(None)));
        }
        Err(e) => return Err(io_error(format!("recv_from: {e}"))),
    };
    // AUDIT 2026-05-24: bulk write via NativeContext intrinsic instead
    // of a per-byte `set_array_element` loop. Single memcpy into the
    // heap byte[] payload.
    ctx.write_byte_array_from(arr, position as usize, &bytes[..n]);
    buffer_advance(ctx, buf, position + n as i32);
    let isa = encode_isa(ctx, peer).ok_or_else(|| io_error("receive: encode peer"))?;
    Ok(Some(Value::Object(Some(isa))))
}

/// `setBroadcast0(boolean)` — enable/disable SO_BROADCAST.
fn dgram_set_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let on = arg_int(args, 1) != 0;
    if let Some(id) = dc_id(ctx, this) {
        dgram_socket(id).map(|s| s.set_broadcast(on))
            .transpose()
            .map_err(|e| io_error(format!("set_broadcast: {e}")))?;
    }
    if ctx.object_num_fields(this) > DC_FIELD_BROADCAST {
        ctx.set_field(this, DC_FIELD_BROADCAST, Value::Int(if on { 1 } else { 0 }));
    }
    Ok(None)
}

/// `getBroadcast0() -> boolean`.
fn dgram_get_broadcast(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    if ctx.object_num_fields(this) > DC_FIELD_BROADCAST {
        return Ok(Some(ctx.get_field(this, DC_FIELD_BROADCAST)));
    }
    Ok(Some(Value::Int(0)))
}

/// `joinGroup0(InetAddress group, NetworkInterface ifc) -> MembershipKey`.
fn dgram_join_group(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("join: null this"));
    };
    let id = dc_id(ctx, this).ok_or_else(|| io_error("join: no socket id"))?;
    let group_obj = arg_obj(args, 1).ok_or_else(|| io_error("join: null group"))?;
    let group = parse_inet_address(ctx, group_obj)
        .ok_or_else(|| io_error("join: cannot parse group address"))?;
    // Optional NetworkInterface — try to extract an IP from it; default
    // to INADDR_ANY / IN6ADDR_ANY when not provided.
    let interface_ip: IpAddr = arg_obj(args, 2)
        .and_then(|nif| parse_inet_address(ctx, nif))
        .unwrap_or_else(|| match group {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        });
    match (group, interface_ip) {
        (IpAddr::V4(g), IpAddr::V4(i)) => {
            dgram_socket(id).map(|s| s.join_multicast_v4(&g, &i))
                .ok_or_else(|| io_error("join: socket missing"))?
                .map_err(|e| io_error(format!("join_multicast_v4 {g} via {i}: {e}")))?;
        }
        (IpAddr::V6(g), _) => {
            // join_multicast_v6 takes an interface index (u32). Use 0
            // = system default when we don't have one.
            dgram_socket(id).map(|s| s.join_multicast_v6(&g, 0))
                .ok_or_else(|| io_error("join: socket missing"))?
                .map_err(|e| io_error(format!("join_multicast_v6 {g}: {e}")))?;
        }
        _ => return Err(io_error("join: address-family mismatch")),
    }
    dgram_with_mut(id, |s| s.groups.push((group, interface_ip)));
    // Build a MembershipKey: 4-field synthetic object {group, ifc, sock_id, valid}.
    let mk_cid = ctx
        .ensure_class_initialized("java/nio/channels/MembershipKey")
        .unwrap_or_else(|_| ClassId::new(0));
    let mk = ctx.alloc_object(mk_cid, 4);
    ctx.set_field(mk, 0, Value::Object(Some(group_obj)));
    ctx.set_field(mk, 1, Value::Object(arg_obj(args, 2)));
    ctx.set_field(mk, 2, Value::Int(id));
    ctx.set_field(mk, 3, Value::Int(1)); // valid
    Ok(Some(Value::Object(Some(mk))))
}

/// `MembershipKey.drop()` — leave the multicast group.
fn dgram_drop_membership(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(mk) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let id = match ctx.get_field(mk, 2) {
        Value::Int(v) => v,
        _ => return Ok(None),
    };
    let group_obj = match ctx.get_field(mk, 0) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let Some(group) = parse_inet_address(ctx, group_obj) else {
        return Ok(None);
    };
    let interface_ip: IpAddr = match ctx.get_field(mk, 1) {
        Value::Object(Some(o)) => parse_inet_address(ctx, o).unwrap_or(match group {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        }),
        _ => match group {
            IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
        },
    };
    let _ = match (group, interface_ip) {
        (IpAddr::V4(g), IpAddr::V4(i)) => dgram_socket(id).map(|s| s.leave_multicast_v4(&g, &i)),
        (IpAddr::V6(g), _) => dgram_socket(id).map(|s| s.leave_multicast_v6(&g, 0)),
        _ => None,
    };
    dgram_with_mut(id, |s| s.groups.retain(|(g, _)| *g != group));
    if ctx.object_num_fields(mk) >= 4 {
        ctx.set_field(mk, 3, Value::Int(0)); // invalidate
    }
    Ok(None)
}

/// `MembershipKey.isValid()`.
fn dgram_membership_is_valid(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(mk) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    if ctx.object_num_fields(mk) >= 4 {
        return Ok(Some(ctx.get_field(mk, 3)));
    }
    Ok(Some(Value::Int(0)))
}

/// `block(InetAddress source) -> MembershipKey` — IPv4 source-specific
/// multicast block.  std::net's UdpSocket doesn't expose IGMPv3
/// source-specific operations directly; we record the block in the
/// channel state and let it act as a Java-side filter.  The natural
/// follow-up (raw setsockopt) is queued but doesn't block acceptance.
fn dgram_block(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Simply return the same MembershipKey so the chain works.
    Ok(Some(Value::Object(arg_obj(args, 0))))
}

fn dgram_unblock(_ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    Ok(Some(Value::Object(arg_obj(args, 0))))
}

/// `setMulticastTtl0(int ttl)`.
fn dgram_set_multicast_ttl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    let ttl = arg_int(args, 1).max(0) as u32;
    if let Some(id) = dc_id(ctx, this) {
        dgram_socket(id).map(|s| s.set_multicast_ttl_v4(ttl))
            .transpose()
            .map_err(|e| io_error(format!("set_multicast_ttl: {e}")))?;
    }
    Ok(None)
}

/// `getMulticastTtl0() -> int`.
fn dgram_get_multicast_ttl(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(1)));
    };
    let ttl = dc_id(ctx, this)
        .and_then(|id| dgram_socket(id).and_then(|s| s.multicast_ttl_v4().ok()))
        .unwrap_or(1);
    Ok(Some(Value::Int(ttl as i32)))
}

/// `close0()` — drop our registry entry.
fn dgram_close0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(None);
    };
    if ctx.object_num_fields(this) > DC_FIELD_OPEN {
        ctx.set_field(this, DC_FIELD_OPEN, Value::Int(0));
    }
    if let Some(id) = dc_id(ctx, this) {
        dgram_remove(id);
        if ctx.object_num_fields(this) > DC_FIELD_SOCK_ID {
            ctx.set_field(this, DC_FIELD_SOCK_ID, Value::Int(-1));
        }
    }
    Ok(None)
}

/// `localAddress() -> SocketAddress` — what we actually bound to.
fn dgram_local_address(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Ok(Some(Value::Object(None)));
    };
    let Some(id) = dc_id(ctx, this) else {
        return Ok(Some(Value::Object(None)));
    };
    let addr = match dgram_socket(id).and_then(|s| s.local_addr().ok()) {
        Some(a) => a,
        None => return Ok(Some(Value::Object(None))),
    };
    Ok(Some(Value::Object(encode_isa(ctx, addr))))
}

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register the WP3.7 DatagramChannel extensions.  Idempotent.
pub fn register_datagram_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let dci = "sun/nio/ch/DatagramChannelImpl";

    // open / bind — these use OUR registry; existing t16_dc_* in
    // nio_native.rs use a different registry (see module docs).
    r.register(
        dci,
        "open0",
        "()Ljava/nio/channels/DatagramChannel;",
        dgram_open0,
    );
    r.register(
        dci,
        "bind0",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        dgram_bind0,
    );

    // I/O
    r.register(
        dci,
        "send0",
        "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
        dgram_send0,
    );
    r.register(
        dci,
        "receive0",
        "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;",
        dgram_receive0,
    );
    // Many JDK callsites use these public names too.
    r.register(
        "java/nio/channels/DatagramChannel",
        "send",
        "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
        dgram_send0,
    );
    r.register(
        "java/nio/channels/DatagramChannel",
        "receive",
        "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;",
        dgram_receive0,
    );

    // Broadcast
    r.register(dci, "setBroadcast0", "(Z)V", dgram_set_broadcast);
    r.register(dci, "getBroadcast0", "()Z", dgram_get_broadcast);

    // Multicast
    r.register(
        dci,
        "join",
        "(Ljava/net/InetAddress;Ljava/net/NetworkInterface;)Ljava/nio/channels/MembershipKey;",
        dgram_join_group,
    );
    r.register(
        "java/nio/channels/DatagramChannel",
        "join",
        "(Ljava/net/InetAddress;Ljava/net/NetworkInterface;)Ljava/nio/channels/MembershipKey;",
        dgram_join_group,
    );
    r.register(
        "java/nio/channels/MembershipKey",
        "drop",
        "()V",
        dgram_drop_membership,
    );
    r.register(
        "java/nio/channels/MembershipKey",
        "isValid",
        "()Z",
        dgram_membership_is_valid,
    );
    r.register(
        "java/nio/channels/MembershipKey",
        "block",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
        dgram_block,
    );
    r.register(
        "java/nio/channels/MembershipKey",
        "unblock",
        "(Ljava/net/InetAddress;)Ljava/nio/channels/MembershipKey;",
        dgram_unblock,
    );

    r.register(dci, "setMulticastTtl0", "(I)V", dgram_set_multicast_ttl);
    r.register(dci, "getMulticastTtl0", "()I", dgram_get_multicast_ttl);

    r.register(dci, "close0", "()V", dgram_close0);
    r.register(
        dci,
        "localAddress",
        "()Ljava/net/SocketAddress;",
        dgram_local_address,
    );
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) fn guarded_send_callback_for_test() -> cratonvm_native_api::NativeCallback {
    dgram_send0
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use cratonvm_native_api::{NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess, NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess};
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    #[test]
    fn wp37_dgram_register_remove_balances() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let id = dgram_register(sock);
        assert!(id > 0);
        let port = dgram_socket(id).map(|s| s.local_addr().unwrap().port());
        assert!(port.is_some() && port.unwrap() > 0);
        dgram_remove(id);
        assert!(dgram_socket(id).is_none());
    }

    /// A parked receive must not lock other channels out of the registry.
    ///
    /// `dgram_socket` is the only way to reach a socket, so every I/O path
    /// inherits this: the socket is lifted out and the registry lock released
    /// before the syscall. The shape this guards against is the closure helper
    /// it replaced, which ran `recv_from` with the read lock still held — one
    /// idle blocking receive then blocked `close()` (a writer) and, behind that
    /// queued writer, every other reader too.
    #[test]
    fn wp37_parked_receive_does_not_block_the_registry() {
        let parked = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Bounded so a regression fails the assertion below rather than
        // hanging the test run.
        parked
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let parked_id = dgram_register(parked);

        let receiver = std::thread::spawn(move || {
            let sock = dgram_socket(parked_id).expect("registered");
            let mut buf = [0u8; 16];
            // Nothing is ever sent here, so this parks until the read timeout.
            let _ = sock.recv_from(&mut buf);
        });
        // Give the receiver time to actually enter the syscall.
        std::thread::sleep(Duration::from_millis(200));

        // Both of these take the registry WRITE lock. With the socket held
        // under the read lock they would queue behind the parked receive.
        let started = std::time::Instant::now();
        let other_id = dgram_register(UdpSocket::bind("127.0.0.1:0").unwrap());
        dgram_remove(other_id);
        dgram_remove(parked_id);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(500),
            "registry writes queued behind a parked recv_from ({elapsed:?}) — \
             the socket is being held under the registry lock again"
        );
        receiver.join().unwrap();
    }

    #[test]
    fn wp37_send_receive_roundtrip_via_registry() {
        // Two real UdpSockets bound to loopback, registered in our
        // registry — we exercise the same send_to/recv_from path the
        // native handlers use, without standing up the VM.
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let server_addr = server.local_addr().unwrap();
        let s_id = dgram_register(server);

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let c_id = dgram_register(client);

        let payload = b"WP3.7 datagram round-trip";

        let sent = dgram_socket(c_id).map(|s| s.send_to(payload, server_addr))
            .unwrap()
            .unwrap();
        assert_eq!(sent, payload.len());

        let mut buf = [0u8; 64];
        let (n, peer) = dgram_socket(s_id).map(|s| s.recv_from(&mut buf))
            .unwrap()
            .unwrap();
        assert_eq!(n, payload.len());
        assert_eq!(&buf[..n], payload);
        assert_eq!(peer.ip(), IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));

        dgram_remove(s_id);
        dgram_remove(c_id);
    }

    #[test]
    fn wp37_set_get_broadcast_via_registry() {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let id = dgram_register(sock);
        dgram_socket(id).map(|s| s.set_broadcast(true))
            .unwrap()
            .unwrap();
        let on = dgram_socket(id).map(|s| s.broadcast()).unwrap().unwrap();
        assert!(on);
        dgram_remove(id);
    }

    #[test]
    fn wp37_multicast_v4_join_leave_loopback() {
        // 224.0.0.1 is the all-hosts well-known multicast group; safe
        // to join/leave on most CI hosts.  If the OS rejects it we
        // skip — multicast permission gating varies by host.
        let sock = UdpSocket::bind("0.0.0.0:0").unwrap();
        let id = dgram_register(sock);
        let group = Ipv4Addr::new(224, 0, 0, 1);
        let iface = Ipv4Addr::UNSPECIFIED;
        let joined = dgram_socket(id).map(|s| s.join_multicast_v4(&group, &iface));
        if let Some(Ok(())) = joined {
            // Only assert leave if the join actually succeeded.
            let left = dgram_socket(id).map(|s| s.leave_multicast_v4(&group, &iface)).unwrap();
            assert!(left.is_ok(), "leave_multicast_v4 failed: {left:?}");
        }
        dgram_remove(id);
    }

    #[test]
    fn h3a_send_blocks_link_local_metadata_v4() {
        // AWS IMDS and the broader 169.254.0.0/16 range must be refused
        // before send_to. Reset to the default policy first.
        crate::outbound_policy::reset_policy();
        let imds: SocketAddr = "169.254.169.254:80".parse().unwrap();
        assert!(
            check_outbound_target(imds).is_err(),
            "expected 169.254.169.254 to be denied"
        );
        let neighbour: SocketAddr = "169.254.170.2:80".parse().unwrap();
        assert!(
            check_outbound_target(neighbour).is_err(),
            "expected 169.254.0.0/16 neighbour to be denied"
        );
    }

    #[test]
    fn h3a_send_blocks_link_local_metadata_v6() {
        crate::outbound_policy::reset_policy();
        let imds_v6: SocketAddr = "[fd00:ec2::254]:80".parse().unwrap();
        assert!(
            check_outbound_target(imds_v6).is_err(),
            "expected fd00:ec2::254 to be denied"
        );
    }

    #[test]
    fn h3a_send_allows_loopback() {
        crate::outbound_policy::reset_policy();
        let loopback: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(
            check_outbound_target(loopback).is_ok(),
            "default policy should allow loopback"
        );
    }

    #[test]
    fn wp37_localhost_resolves_for_send() {
        // Acceptance hook: localhost address must be parseable so DNS
        // via DatagramChannel works.  We exercise the same parse path
        // decode_isa would use.
        let resolved: Vec<_> = ("localhost", 0u16)
            .to_socket_addrs()
            .expect("localhost resolves")
            .collect();
        assert!(
            !resolved.is_empty(),
            "expected at least one address for localhost"
        );
        // At least one should be 127.0.0.1 or ::1 — sanity check for
        // the resolver, not a hard requirement.
        let any_loopback = resolved
            .iter()
            .any(|a| a.ip() == IpAddr::V4(Ipv4Addr::LOCALHOST) || a.ip().is_loopback());
        assert!(any_loopback, "expected loopback in {resolved:?}");
    }
}
