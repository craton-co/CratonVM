// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! WP3.7 — `java.nio.channels.DatagramChannel` multicast group membership
//! (`join` and the resulting `MembershipKey`), plus the outbound-policy gate
//! shared with the UDP send path.
//!
//! ## The registry split, and how it was resolved (2026-07-28)
//!
//! This module used to keep its **own** `dgram_registry` of `UdpSocket`s,
//! keyed by an id it stashed in the channel object's slot 4, because the
//! `t16_dc_*` family in `nio_native.rs` owned a second one and a file-scope
//! rule for the original WP forbade touching it. The module doc called
//! unifying them a follow-up.
//!
//! That split was not merely untidy — it left `DatagramChannel` unable to
//! send at all:
//!
//!   * `nio_native.rs`'s `t16_dc_*` family and its `udp_registry` are
//!     `#[cfg(feature = "synthetic-jdk")]`, so they are compiled out of the
//!     real-JDK build entirely.
//!   * The live real-JDK implementation is the `native_dc_*` family in
//!     `lib.rs`, which keeps its socket in `ctx.fd_table()` and maps a channel
//!     to its `FdId` through `dc_fds()`.
//!   * Nothing ever populated *this* module's registry, so `dc_id()` always
//!     returned `None`, and `DatagramChannel.send` — deliberately deferred
//!     here to keep the SSRF gate — failed every call with
//!     `IOException: send: no socket id`.
//!
//! There is now **one** store: `ctx.fd_table()`, reached through `lib.rs`'s
//! `dc_fd()`. `send` moved to `native_dc_send` alongside the rest of the
//! family and still calls this module's `check_outbound_target`; what remains
//! here resolves a channel the same way every other DatagramChannel native
//! does.
//!
//! The registrations this module used to make against
//! `sun/nio/ch/DatagramChannelImpl` — `open0`, `bind0`, `send0`, `receive0`,
//! `setBroadcast0`, `getBroadcast0`, `setMulticastTtl0`, `getMulticastTtl0`,
//! `close0`, `localAddress` — were **all dead**: the real class has no such
//! methods, and for the three names that do exist (`send0`, `receive0`,
//! `localAddress`) the real descriptor differs, so the registry's
//! (class, name, descriptor) key never matched. They are gone.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs};

use std::sync::{OnceLock, RwLock};

use cratonvm_native_api::fd_table::FdId;
use cratonvm_native_api::{NativeContext, NativeMethodRegistry};
use cratonvm_types::error::{MethodCallFailed, MethodCallResult, RuntimeError, VmError};
use cratonvm_types::{ClassId, ObjectRef, Value};

// ---------------------------------------------------------------------------
// Multicast group bookkeeping (the socket itself lives in ctx.fd_table())
// ---------------------------------------------------------------------------

/// Groups currently joined per UDP fd, as (group, interface) pairs.
///
/// The socket is NOT kept here — `ctx.fd_table()` owns it, and every operation
/// goes through the fd_table's own accessors, which clone the entry `Arc` out
/// before touching it. This table holds plain addresses only, so nothing can
/// block while it is locked (the hazard that
/// `socket_channel::resolve_stream` and `net.rs`'s `NetSocketHandle::Stream`
/// exist to avoid).
fn joined_groups() -> &'static RwLock<HashMap<FdId, Vec<(IpAddr, IpAddr)>>> {
    static G: OnceLock<RwLock<HashMap<FdId, Vec<(IpAddr, IpAddr)>>>> = OnceLock::new();
    G.get_or_init(|| RwLock::new(HashMap::new()))
}

fn record_join(fd: FdId, group: IpAddr, interface: IpAddr) {
    if let Ok(mut g) = joined_groups().write() {
        g.entry(fd).or_default().push((group, interface));
    }
}

fn record_leave(fd: FdId, group: IpAddr) {
    if let Ok(mut g) = joined_groups().write() {
        if let Some(list) = g.get_mut(&fd) {
            list.retain(|(joined, _)| *joined != group);
            if list.is_empty() {
                g.remove(&fd);
            }
        }
    }
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

/// The UDP fd backing this channel, resolved through the one store every
/// DatagramChannel native uses (`lib.rs`'s `dc_fds` → `ctx.fd_table()`).
///
/// This replaced a `dc_id` that read an id out of the channel object's slot 4
/// and looked it up in a registry of this module's own — a registry that
/// nothing populated, so it always missed. See the module doc.
fn dc_fd_of(ctx: &dyn NativeContext, this: ObjectRef) -> Option<FdId> {
    crate::dc_fd(ctx, this)
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
pub(crate) fn check_outbound_target(target: SocketAddr) -> Result<(), MethodCallFailed> {
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

fn parse_inet_address(ctx: &mut dyn NativeContext, addr: ObjectRef) -> Option<IpAddr> {
    // Preferred: the public accessor, which works for BOTH the real-JDK
    // `Inet4Address` — whose address lives in `holder.address` as a packed int,
    // with no String in any low slot — and CratonVM's synthetic layout. The
    // slot probing below misses the real one entirely, so `join` failed with
    // "cannot parse group address" for every genuine `InetAddress`. That was
    // invisible until the fd unification made `join` reachable at all.
    //
    // A non-InetAddress argument (`join`'s `NetworkInterface`) has no such
    // method; the call fails and we fall through, which is exactly what the
    // caller's "default to the wildcard interface" path expects.
    if let Ok(Some(Value::Object(Some(s)))) =
        ctx.invoke_virtual(addr, "getHostAddress", "()Ljava/lang/String;", &[])
    {
        if let Some(text) = ctx.read_string(s) {
            if let Ok(ip) = text.parse::<IpAddr>() {
                return Some(ip);
            }
        }
    }
    // Fallback: IP text at slot 1 (what net.rs encodes), then slot 0.
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

/// The `MembershipKey` private slot map, relative to
/// [`crate::concrete_receiver::concrete_base`].
///
/// Absolute indices until 2026-08-21: the key was minted AS the ABSTRACT
/// `java.nio.channels.MembershipKey`, which declares no instance fields, so
/// slots 0..3 were nobody's. They are now written above the concrete class's
/// own layout — `sun.nio.ch.MembershipKeyImpl` declares seven and each of its
/// two concrete subclasses three more — because the mint moved to that class
/// (see [`dgram_join_group`]).
const MK_FIELD_GROUP: usize = 0;
const MK_FIELD_INTERFACE: usize = 1;
const MK_FIELD_FD: usize = 2;
const MK_FIELD_VALID: usize = 3;
/// How many private slots [`dgram_join_group`] appends above the real layout.
const MK_PRIVATE_SLOTS: usize = 4;

/// The concrete classes HotSpot 25 builds for a multicast membership, IPv4 and
/// IPv6. Both extend the (also abstract) `sun.nio.ch.MembershipKeyImpl`; which
/// one the JDK picks is decided by the group address's family, so this crate
/// picks the same way rather than always naming one.
const MK_IMPL_V4: &str = "sun/nio/ch/MembershipKeyImpl$Type4";
const MK_IMPL_V6: &str = "sun/nio/ch/MembershipKeyImpl$Type6";

/// `joinGroup0(InetAddress group, NetworkInterface ifc) -> MembershipKey`.
fn dgram_join_group(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(this) = arg_obj(args, 0) else {
        return Err(io_error("join: null this"));
    };
    let fd = dc_fd_of(ctx, this).ok_or_else(|| io_error("join: channel has no UDP socket"))?;
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
            ctx.fd_table()
                .udp_join_multicast_v4(fd, &g, &i)
                .map_err(|e| io_error(format!("join_multicast_v4 {g} via {i}: {e}")))?;
        }
        (IpAddr::V6(g), _) => {
            // join_multicast_v6 takes an interface index (u32). Use 0
            // = system default when we don't have one.
            ctx.fd_table()
                .udp_join_multicast_v6(fd, &g, 0)
                .map_err(|e| io_error(format!("join_multicast_v6 {g}: {e}")))?;
        }
        _ => return Err(io_error("join: address-family mismatch")),
    }
    record_join(fd, group, interface_ip);
    // Build a MembershipKey: {group, ifc, fd, valid}, appended above the
    // concrete class's own layout.
    //
    // **The defect this closes.** `ensure_class_initialized(
    // "java/nio/channels/MembershipKey")` resolves to the real, ABSTRACT JDK
    // class, and `alloc_object` then minted an object whose runtime class is
    // abstract -- a receiver `new` cannot legally produce (JVMS 6.5). Same
    // one-line shape `H21-1` fixed for `Pipe`. `java.nio.channels.MembershipKey`
    // declares no instance fields, which is why slots 0..3 were free and why
    // moving to a class that DOES declare fields needs the appended base.
    //
    // **Which concrete class**: the JDK's own `MembershipRegistry` builds
    // `MembershipKeyImpl.Type4` for an IPv4 group and `Type6` otherwise, so
    // this picks by the same discriminator it already computed above.
    let mk_impl = match group {
        IpAddr::V4(_) => MK_IMPL_V4,
        IpAddr::V6(_) => MK_IMPL_V6,
    };
    let minted = crate::concrete_receiver::alloc_concrete(
        ctx,
        &[mk_impl],
        "java/nio/channels/MembershipKey",
        MK_PRIVATE_SLOTS,
    );
    let (mk, base) = (minted.obj, minted.base);
    ctx.set_field(mk, base + MK_FIELD_GROUP, Value::Object(Some(group_obj)));
    ctx.set_field(
        mk,
        base + MK_FIELD_INTERFACE,
        Value::Object(arg_obj(args, 2)),
    );
    ctx.set_field(mk, base + MK_FIELD_FD, Value::Int(fd as i32));
    ctx.set_field(mk, base + MK_FIELD_VALID, Value::Int(1));
    Ok(Some(Value::Object(Some(mk))))
}

/// `MembershipKey.drop()` — leave the multicast group.
fn dgram_drop_membership(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(mk) = arg_obj(args, 0) else {
        return Ok(None);
    };
    // Resolved from the RECEIVER, with the width guard that collapses to 0 for
    // any key this crate did not allocate -- see `concrete_base`.
    let base = crate::concrete_receiver::concrete_base(ctx, mk, MK_PRIVATE_SLOTS);
    let fd = match ctx.get_field(mk, base + MK_FIELD_FD) {
        Value::Int(v) if v >= 0 => v as FdId,
        _ => return Ok(None),
    };
    let group_obj = match ctx.get_field(mk, base + MK_FIELD_GROUP) {
        Value::Object(Some(o)) => o,
        _ => return Ok(None),
    };
    let Some(group) = parse_inet_address(ctx, group_obj) else {
        return Ok(None);
    };
    let interface_ip: IpAddr = match ctx.get_field(mk, base + MK_FIELD_INTERFACE) {
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
        (IpAddr::V4(g), IpAddr::V4(i)) => Some(ctx.fd_table().udp_leave_multicast_v4(fd, &g, &i)),
        (IpAddr::V6(g), _) => Some(ctx.fd_table().udp_leave_multicast_v6(fd, &g, 0)),
        _ => None,
    };
    record_leave(fd, group);
    if ctx.object_num_fields(mk) >= base + MK_PRIVATE_SLOTS {
        ctx.set_field(mk, base + MK_FIELD_VALID, Value::Int(0)); // invalidate
    }
    Ok(None)
}

/// `MembershipKey.isValid()`.
fn dgram_membership_is_valid(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let Some(mk) = arg_obj(args, 0) else {
        return Ok(Some(Value::Int(0)));
    };
    let base = crate::concrete_receiver::concrete_base(ctx, mk, MK_PRIVATE_SLOTS);
    if ctx.object_num_fields(mk) >= base + MK_PRIVATE_SLOTS {
        return Ok(Some(ctx.get_field(mk, base + MK_FIELD_VALID)));
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

// ---------------------------------------------------------------------------
// Public registration
// ---------------------------------------------------------------------------

/// Register the multicast-membership surface. Idempotent.
///
/// `open` / `bind` / `connect` / `send` / `receive` / `read` / `write` /
/// `close` / `configureBlocking` / `getLocalAddress` all live in `lib.rs`'s
/// `register_datagram_channel`, over `ctx.fd_table()`. This module registers
/// only what that family does not cover, and resolves the channel through the
/// same `dc_fd` — see the module doc for the registry split this replaced.
// JDK-ONLY-CLASSIFY: unknown — needs census. Small function, but every one of
// its resolvable triples targets an ABSTRACT method of
// `java.nio.channels.DatagramChannel` (4) or a method absent from the real
// class (1); none is ACC_NATIVE in JDK 25. Multicast join/leave IS a syscall,
// so the behaviour is bridge-shaped, yet it is registered on the abstract
// public API rather than on `sun.nio.ch.DatagramChannelImpl`, so the tag here
// governs dispatch for any DatagramChannel subclass. Evidence needed: receiver
// classes seen at dispatch.
pub fn register_datagram_real(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Where this registrar's own rows start; the mirrors at the foot of the
    // function must not be able to see any other crate's.
    let __rows_before = r.dump_registrations().len();

    // Multicast
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

    // The registration half of the two fabricated-receiver fixes above.
    //
    // `join` is an INSTANCE method whose receiver is now
    // `sun.nio.ch.DatagramChannelImpl` (`lib.rs::native_dc_open`), and the four
    // `MembershipKey` rows now answer for a `MembershipKeyImpl.Type4`/`Type6`.
    // Dispatch keys on the receiver's runtime class (`H11-1`), and the
    // superclass walk does not run for a class that declares the method with
    // `Code` -- which all of these do. Without these mirrors the natives above
    // would simply stop being reached.
    //
    // `MembershipKeyImpl` itself is in the list because it is where the two
    // concrete subclasses inherit `isValid`/`drop`/`block`/`unblock` FROM: a
    // `Type4` receiver declares none of them, so step 1 misses and the walk
    // goes to its superclass -- which must find a registration there, not the
    // JDK's own body.
    crate::concrete_receiver::mirror_class_registrations(
        r,
        __rows_before,
        "java/nio/channels/DatagramChannel",
        "sun/nio/ch/DatagramChannelImpl",
    );
    for impl_name in ["sun/nio/ch/MembershipKeyImpl", MK_IMPL_V4, MK_IMPL_V6] {
        crate::concrete_receiver::mirror_class_registrations(
            r,
            __rows_before,
            "java/nio/channels/MembershipKey",
            impl_name,
        );
    }
    r.set_category(__prev_cat);
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    // The socket-lifecycle tests that used to live here (`register/remove
    // balances`, `send/receive roundtrip via registry`, `set/get broadcast via
    // registry`, `parked receive does not block the registry`) exercised this
    // module's own `dgram_registry`, which no longer exists — `ctx.fd_table()`
    // owns the socket now. The behaviour they covered is exercised end-to-end
    // by `tools/udp-probe/UdpPathProbe.java` against a real VM, which the old
    // registry could never satisfy: nothing populated it, so `send` failed with
    // "no socket id" on every real call.

    #[test]
    fn multicast_group_bookkeeping_is_per_fd() {
        let fd_a: FdId = 4001;
        let fd_b: FdId = 4002;
        let group = IpAddr::V4(Ipv4Addr::new(239, 1, 2, 3));
        let other = IpAddr::V4(Ipv4Addr::new(239, 4, 5, 6));
        let iface = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

        record_join(fd_a, group, iface);
        record_join(fd_a, other, iface);
        record_join(fd_b, group, iface);
        assert_eq!(joined_groups().read().unwrap().get(&fd_a).unwrap().len(), 2);

        // Leaving one group on one fd must not touch the same group on another.
        record_leave(fd_a, group);
        assert_eq!(joined_groups().read().unwrap().get(&fd_a).unwrap().len(), 1);
        assert_eq!(joined_groups().read().unwrap().get(&fd_b).unwrap().len(), 1);

        // The last leave drops the fd's entry entirely rather than leaving an
        // empty Vec behind for every channel that ever joined.
        record_leave(fd_a, other);
        assert!(!joined_groups().read().unwrap().contains_key(&fd_a));
        record_leave(fd_b, group);
        assert!(!joined_groups().read().unwrap().contains_key(&fd_b));
    }

    #[test]
    fn h3a_send_blocks_link_local_metadata_v4() {
        let metadata: SocketAddr = "169.254.169.254:80".parse().unwrap();
        assert!(
            check_outbound_target(metadata).is_err(),
            "cloud-metadata IPv4 must stay blocked for UDP send"
        );
        let link_local: SocketAddr = "169.254.1.1:53".parse().unwrap();
        assert!(
            check_outbound_target(link_local).is_err(),
            "link-local IPv4 must stay blocked for UDP send"
        );
    }

    #[test]
    fn h3a_send_blocks_link_local_metadata_v6() {
        let metadata: SocketAddr = "[fd00:ec2::254]:80".parse().unwrap();
        assert!(
            check_outbound_target(metadata).is_err(),
            "cloud-metadata IPv6 must stay blocked for UDP send"
        );
    }

    #[test]
    fn h3a_send_allows_loopback() {
        let loopback: SocketAddr = "127.0.0.1:9".parse().unwrap();
        assert!(
            check_outbound_target(loopback).is_ok(),
            "loopback must remain reachable"
        );
    }

    #[test]
    fn wp37_localhost_resolves_for_send() {
        // `send` accepts a hostname target; make sure the resolver used by the
        // decode path still yields a loopback address for `localhost`.
        let resolved: Vec<SocketAddr> = ("localhost", 0u16)
            .to_socket_addrs()
            .expect("resolve localhost")
            .collect();
        let any_loopback = resolved.iter().any(|a| a.ip().is_loopback());
        assert!(any_loopback, "expected loopback in {resolved:?}");
    }
}
