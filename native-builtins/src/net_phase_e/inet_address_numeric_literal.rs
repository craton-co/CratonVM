#[allow(unused_imports)]
use super::*;

/// The numeric literal of an `InetAddress`, from whichever of the three
/// layouts this VM can present: the ObjectRef-keyed side table, a real-JDK
/// `Inet6Address.holder6.ipaddress` byte array, or a real-JDK
/// `InetAddress.holder.address` packed v4 int.
///
/// IPv6 is checked BEFORE the v4 int because an `Inet6Address` leaves that int
/// at zero — reading it would render every v6 address as `0.0.0.0`, i.e. the
/// v4 wildcard. The v6 form is bracketed so a caller appending `:{port}`
/// produces something `ToSocketAddrs` can parse.
pub fn inet_address_numeric_literal(ctx: &dyn NativeContext, ia: ObjectRef) -> Option<String> {
    if let Some((_, ip)) = inet_addr_get(ia) {
        if !ip.is_empty() {
            return Some(bracket_if_v6(&ip));
        }
    }
    if let Value::Object(Some(h6)) = ctx.get_field_by_name(ia, "holder6") {
        if let Value::Object(Some(arr)) = ctx.get_field_by_name(h6, "ipaddress") {
            if ctx.array_length(arr) == 16 {
                let mut octets = [0u8; 16];
                for (i, slot) in octets.iter_mut().enumerate() {
                    match ctx.get_array_element(arr, i) {
                        Value::Int(b) => *slot = b as u8,
                        _ => return None,
                    }
                }
                return Some(format!("[{}]", std::net::Ipv6Addr::from(octets)));
            }
        }
    }
    if let Value::Object(Some(holder)) = ctx.get_field_by_name(ia, "holder") {
        if let Value::Int(address) = ctx.get_field_by_name(holder, "address") {
            return Some(std::net::Ipv4Addr::from((address as u32).to_be_bytes()).to_string());        }    }
    None
}

/// Wrap a bare IPv6 literal in brackets so `format!("{host}:{port}")` stays
/// parseable; leave anything else (a v4 literal, an already-bracketed form)
/// exactly as it is.
pub fn bracket_if_v6(ip: &str) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]")
    } else {
        ip.to_string()
    }
}

/// Apply the socket options the JDK's own `DatagramSocket` implementation
/// turns on at construction.
///
/// `java.net.DatagramSocket` is a `DatagramChannel` adaptor on JDK 25, and
/// `DatagramChannelImpl`'s constructor sets `SO_BROADCAST` for the adaptor
/// case. Measured on HotSpot 25, same host:
/// `new DatagramSocket().getBroadcast()` is **true**. CratonVM's ctors left
/// the raw socket at the OS default (false), so a caller that never touches
/// `setBroadcast` got a socket that silently dropped broadcast sends where the
/// JDK's would deliver them — and `getBroadcast()` answered a defensible-looking
/// `false` that nothing contradicted until this class was censused against
/// HotSpot method by method.
///
/// Best-effort: a platform that refuses the option leaves the socket as it was,
/// which is the pre-fix behaviour.
pub fn ds_socket_defaults(ctx: &dyn NativeContext, fd: u32) {
    let _ = ctx.fd_table().udp_set_broadcast(fd, true);
}

pub fn register_re7_datagram_socket(r: &mut NativeMethodRegistry) {
    let ds = "java/net/DatagramSocket";

    r.register(ds, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // gc-common w17-a: close the descriptors of sockets this VM dropped
        // open (allocates nothing, so `this` stays current).
        net_release_deferred(ctx);
        // GAP I6 (UDP half): a datagram bind is a network authority too.
        // DUAL-STACK wildcard, matching the JDK's DatagramChannel adaptor —
        // see `open_udp_wildcard_dual_stack_gated`.
        let fd =
            crate::capability_gate::open_udp_wildcard_dual_stack_gated(&*ctx, 0).map_err(|e| {
                crate::capability_gate::translate_open_failure(e, |io| format!("UDP open: {io}"))
            })?;
        let port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(0);
        ds_socket_defaults(&*ctx, fd);
        ds_set(ctx.vm_identity(), this, |s| {
            s.port = port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;        });
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        net_release_deferred(ctx);
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        // GAP I6 (UDP half). Dual-stack, as above: `new DatagramSocket(port)`
        // is a wildcard bind and the JDK's is AF_INET6 with V6ONLY off.
        let fd = crate::capability_gate::open_udp_wildcard_dual_stack_gated(
            &*ctx,
            port.clamp(0, 65535) as u16,
        )
        .map_err(|e| {
            crate::capability_gate::translate_open_failure(e, |io| format!("UDP bind: {io}"))
        })?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(port);
        ds_socket_defaults(&*ctx, fd);
        ds_set(ctx.vm_identity(), this, |s| {
            s.port = actual_port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;        });
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "<init>", "(ILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        net_release_deferred(ctx);
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let host = match args.get(2) {
            Some(Value::Object(Some(ia))) => {
                inet_addr_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0")
            }
            _ => "0.0.0.0".to_string(),
        };
        // GAP I6 (UDP half).
        let fd = crate::capability_gate::open_udp_gated(&*ctx, Some(&format!("{host}:{port}")))
            .map_err(|e| {
                crate::capability_gate::translate_open_failure(e, |io| format!("UDP bind: {io}"))
            })?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
            .unwrap_or(port);
        ds_socket_defaults(&*ctx, fd);
        ds_set(ctx.vm_identity(), this, |s| {
            s.port = actual_port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;        });
        Ok(None)
    }, NativeKind::SyntheticStub);

    // setOption / getOption.
    //
    // `java.net.DatagramSocket.setOption` is `delegate().setOption(..)`, and a
    // CratonVM datagram socket has no delegate, so every call landed in the
    // JDK InternalError("Should not get here") -- including
    // `setOption(IP_DONTFRAGMENT, ..)`, which is the ONE extended option not
    // gated by a native support probe and therefore the one applications
    // actually reach. Implementing the option surface here is what makes the
    // `jdk/net/*SocketOptions` family reachable from a DatagramSocket at all.
    //
    // Dispatch is by `SocketOption.name()`, the JDK own identity for these
    // (both `StandardSocketOptions` and `ExtendedSocketOptions` name them), so
    // one arm serves whichever constant object the caller passes.
    r.register(
        ds,
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/net/DatagramSocket;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // See `getOption` below: `Objects.requireNonNull(name)` first.
            if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                return Err(npe_no_message());
            }
            // gc-common w20-d: the value is unboxed BEFORE `name()` (Java);
            // it was read from `args`, which is not remapped, after it.
            let value = args.get(2).copied().unwrap_or(Value::Object(None));
            let on = ds_unbox_bool(ctx, value);
            let n = ds_unbox_int(ctx, value);
            let name = ds_socket_option_name(ctx, &mut this, args.get(1).copied());
            let fd = ds_get(this).fd;
            if fd < 0 {
                return Err(ioex("DatagramSocket: closed"));
            }
            let outcome = match name.as_str() {
                "IP_DONTFRAGMENT" => ds_set_dont_fragment(ctx, fd, on),
                "SO_BROADCAST" => {
                    let r = ctx.fd_table().udp_set_broadcast(fd as u32, on);
                    if r.is_ok() {
                        ds_set(ctx.vm_identity(), this, |sd| sd.broadcast = i32::from(on));
                    }
                    r
                }
                "SO_REUSEADDR" => {
                    let r = ctx.fd_table().udp_set_reuse_address(fd as u32, on);
                    if r.is_ok() {
                        ds_set(ctx.vm_identity(), this, |sd| {
                            sd.reuse_address = i32::from(on)
                        });
                    }
                    r
                }
                "SO_RCVBUF" => ctx
                    .fd_table()
                    .udp_set_recv_buffer_size(fd as u32, n.max(0) as usize),
                "SO_SNDBUF" => ctx
                    .fd_table()
                    .udp_set_send_buffer_size(fd as u32, n.max(0) as usize),
                "IP_TOS" => ctx.fd_table().udp_set_tos(fd as u32, n.max(0) as u32),
                "IP_MULTICAST_TTL" => ctx
                    .fd_table()
                    .udp_set_multicast_ttl_v4(fd as u32, n.max(0) as u32),
                _ => {
                    // The JDK throws for an option its provider does not
                    // support; naming it is what lets a caller tell that from
                    // a failure to apply one we do support. The wording is
                    // `sun.nio.ch.Net`'s own — `"'" + name + "' not
                    // supported"` — because a caller that string-matches the
                    // JDK's text (or a test that asserts it) sees the same
                    // sentence here.
                    return Err(RuntimeError::UnsupportedOperationException {
                        message: format!("'{name}' not supported"),
                    }
                    .into());
                }
            };
            outcome.map_err(|e| ioex(format!("{name}: {e}")))?;
            Ok(Some(Value::Object(Some(this))))
        }, NativeKind::SyntheticStub,
    );
    r.register(
        ds,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // `getOption`/`setOption` open with `Objects.requireNonNull(name)`,
            // which throws a MESSAGELESS NPE. Reading the name off a null
            // option produced `""` and then an
            // `UnsupportedOperationException` naming the empty string — the
            // wrong type, for a caller who passed null by mistake.
            if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                return Err(npe_no_message());
            }
            let name = ds_socket_option_name(ctx, &mut this, args.get(1).copied());
            let sd = ds_get(this);
            if sd.fd < 0 {
                return Err(ioex("DatagramSocket: closed"));
            }
            match name.as_str() {
                "IP_DONTFRAGMENT" => {
                    let on = ds_get_dont_fragment(ctx, sd.fd);
                    ds_box_bool(ctx, on)
                }
                "SO_BROADCAST" => ds_box_bool(ctx, sd.broadcast == 1),
                "SO_REUSEADDR" => ds_box_bool(ctx, sd.reuse_address == 1),
                "SO_TIMEOUT" => ds_box_int(ctx, sd.timeout),
                // The three the SETTER already pushes to the real fd, and the
                // table can read straight back. Refusing them here made
                // `getOption(SO_RCVBUF)` throw on a socket whose
                // `setOption(SO_RCVBUF, n)` had just succeeded — an asymmetry
                // no JDK provider has.
                "SO_RCVBUF" => {
                    let n = ctx
                        .fd_table()
                        .udp_recv_buffer_size(sd.fd as u32)
                        .map_err(|e| ioex(format!("SO_RCVBUF: {e}")))?;
                    ds_box_int(ctx, n as i32)
                }
                "SO_SNDBUF" => {
                    let n = ctx
                        .fd_table()
                        .udp_send_buffer_size(sd.fd as u32)
                        .map_err(|e| ioex(format!("SO_SNDBUF: {e}")))?;
                    ds_box_int(ctx, n as i32)
                }
                "IP_TOS" => {
                    let n = ctx
                        .fd_table()
                        .udp_tos(sd.fd as u32)
                        .map_err(|e| ioex(format!("IP_TOS: {e}")))?;
                    ds_box_int(ctx, n as i32)
                }
                _ => Err(RuntimeError::UnsupportedOperationException {
                    message: format!("'{name}' not supported"),
                }
                .into()),
            }
        }, NativeKind::SyntheticStub,
    );

    // isBound / getLocalAddress / setBroadcast / getBroadcast — the four keys
    // the phase-72 `java/net/DatagramSocket` set owned alone. They are here now
    // so this registrar covers the whole class in BOTH builds: phase-72 is
    // `#[cfg(feature = "synthetic-jdk")]`, so in the default build these four
    // did not exist at all and resolved to the abstract declaration.
    r.register(ds, "isBound", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let sd = ds_get(this);
        // A `DatagramSocket` is bound from construction: every ctor here opens
        // and binds a real UDP fd. It stays bound after close, which is what
        // the JDK specifies.
        Ok(Some(Value::Int(i32::from(sd.fd >= 0 || sd.closed != 0))))
    }, NativeKind::SyntheticStub);
    r.register(
        ds,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sd = ds_get(this);
            if sd.closed != 0 || sd.fd < 0 {
                // "If the socket is closed, returns null" — and an unbound
                // socket answers the wildcard, which is what the fd reports.
                return Ok(Some(Value::Object(None)));
            }
            // `udp_origin_split`, not a bare `rsplit_once(':')`: the split
            // has to survive a v6 address. Since the wildcard socket is
            // AF_INET6 the table reports `[::]:PORT`, and the naive split
            // kept the BRACKETS — `getLocalAddress()` answered `/[::]` where
            // HotSpot answers `/0:0:0:0:0:0:0:0`. `udp_origin_split` parses
            // through `SocketAddr` first, so the host comes back as the
            // address rather than as its textual wrapper.
            let addr = ctx
                .fd_table()
                .udp_local_addr(sd.fd as u32)
                .ok()
                .and_then(|s| udp_origin_split(&s).map(|(h, _)| h))
                .unwrap_or_else(|| "0.0.0.0".to_string());
            // The UDP socket's own bound address, read back as numeric text.
            let ia = alloc_inet_address_unnamed(ctx, &addr)?;
            Ok(Some(Value::Object(Some(ia))))
        }, NativeKind::SyntheticStub,
    );
    r.register(ds, "setBroadcast", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let fd = ds_get(this).fd;
        if fd < 0 {
            return Err(ioex("DatagramSocket: closed"));
        }
        ctx.fd_table()
            .udp_set_broadcast(fd as u32, on)
            .map_err(|e| ioex(format!("SO_BROADCAST: {e}")))?;
        ds_set(ctx.vm_identity(), this, |sd| sd.broadcast = i32::from(on));
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "getBroadcast", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sd = ds_get(this);
        // Ask the SOCKET when nobody has called the setter. The old comment
        // here asserted "the JDK default, which is false"; measured against
        // HotSpot 25 on the same host, `new DatagramSocket().getBroadcast()`
        // is **true** — the JDK's DatagramSocket is a DatagramChannel adaptor
        // and enables SO_BROADCAST when it opens. A hardcoded `false`
        // therefore contradicted an option the socket really had on.
        if sd.broadcast < 0 && sd.fd >= 0 {
            if let Ok(on) = ctx.fd_table().udp_broadcast(sd.fd as u32) {
                return Ok(Some(Value::Int(i32::from(on))));
            }
        }
        Ok(Some(Value::Int(i32::from(sd.broadcast == 1))))
    }, NativeKind::SyntheticStub);
    // (A byte-identical SECOND copy of the four registrations above stood here
    // and was removed. It was inert — last-write-wins with the same closure —
    // but it is the exact shape the `connect`/`disconnect` note below records
    // going wrong once already: two copies that drift silently, with the
    // compiler seeing nothing.)

    // `connect(SocketAddress)` — the overload the pair further down does not
    // cover. Its `(InetAddress,int)` sibling and `disconnect()`/`isConnected()`
    // are registered there, and this file must not carry two of any of them:
    // registration is last-write-wins, so a duplicate is silently dead and the
    // two copies drift. (They did: the second `disconnect` picked up a stray
    // `connected = 1` while the two independent closures of this list were
    // merged, and `isConnected()` then read `false` after `connect()` and
    // `true` after `disconnect()` — backwards, and invisible to the compiler.)
    r.register(ds, "connect", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The JDK's three refusals, in its order:
        //   null                       -> IllegalArgumentException "Address can't be null"
        //   not an InetSocketAddress   -> IllegalArgumentException "Unsupported address type"
        //   InetSocketAddress.createUnresolved(..) -> SocketException "Unresolved address"
        // The third is the one that mattered: an unresolved address reached
        // the resolver and came back as a bare `IOException` naming
        // getaddrinfo, so a caller could not tell "you passed me a hostname I
        // was never asked to resolve" from "the network is down".
        let Some(Value::Object(Some(sa))) = args.get(1).copied() else {
            return Err(iae("Address can't be null"));
        };
        if matches!(
            ctx.invoke_virtual(sa, "isUnresolved", "()Z", &[]),
            Ok(Some(Value::Int(1)))
        ) {
            return Err(socket_ex(ctx, "Unresolved address"));
        }
        // Same real-vs-synthetic layout trap `dp_layout` covers for
        // `DatagramPacket`, one class over: a real-JDK
        // `java.net.InetSocketAddress` declares ONE instance field
        // (`holder`), so the slot-0-is-host / slot-1-is-port read this
        // replaces answered the host as an unreadable holder object ("")
        // and the port as an out-of-layout `Int(0)` — i.e. every
        // `connect(new InetSocketAddress(h, p))` targeted `127.0.0.1:0`.
        // `read_inet_socket_address` already knows both layouts and is
        // what the `Socket.connect` path uses.
        let (host, port) = read_inet_socket_address(ctx, sa)?;
        let host = if host.is_empty() {
            "127.0.0.1".to_string()
        } else {
            host
        };
        let fd = ds_get(this).fd;
        if fd < 0 {
            return Err(ioex("DatagramSocket: closed"));
        }
        ctx.fd_table()
            .udp_connect(fd as u32, &format!("{host}:{port}"))
            .map_err(|e| ioex(format!("UDP connect: {e}")))?;
        ds_set(ctx.vm_identity(), this, |sd| sd.connected = 1);
        ds_set_peer(ctx.vm_identity(), this, &host, port);
        Ok(None)
    }, NativeKind::SyntheticStub);

    r.register(ds, "send", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `DatagramSocket.send(DatagramPacket p)` opens with `synchronized
        // (p)`, so a null packet is a MONITORENTER on null and HotSpot's
        // helpful-NPE names the parameter. `obj_arg`'s generic "null object
        // argument" said the same thing in words no JDK ever prints.
        let Some(Value::Object(Some(pkt))) = args.get(1).copied() else {
            return Err(npe(
                "Cannot enter synchronized block because \"p\" is null".to_string()
            ));
        };
        ds_require_not_closed(ctx, this)?;
        let fd = ds_require_open_socket(ctx, this)?;
        let fd = fd as i32;
        let lay = dp_layout(ctx, pkt);
        let data_arr = match ctx.get_field(pkt, lay.buf) {
            Value::Object(Some(a)) => a,
            _ => return Err(ioex("DatagramPacket: null data")),
        };
        // The real JDK sends `buf[offset .. offset+length]`. The synthetic
        // layout reports no offset and this stays 0, as before.
        let off = lay
            .offset
            .and_then(|s| ctx.get_field(pkt, s).as_int())
            .unwrap_or(0)
            .max(0);
        let len = ctx.get_field(pkt, lay.length).as_int().unwrap_or(0);
        let port = ctx.get_field(pkt, lay.port).as_int().unwrap_or(0);
        let host = match ctx.get_field(pkt, lay.address) {
            Value::Object(Some(ia)) => inet_addr_field_string_or(ctx, ia, IA_ADDR, ""),
            _ => String::new(),
        };
        if host.is_empty() || !(1..=65535).contains(&port) {
            return Err(iae(format!("DatagramPacket: bad addr {host}:{port}")));
        }
        // A connected socket may only send to its peer: "if this socket is
        // connected and the packet's address does not match the connected
        // address, or if the packet's port does not match the connected port,
        // an IllegalArgumentException is thrown". Without the check the
        // datagram went to the packet's address, which is the opposite of
        // what connecting a socket is for.
        if ds_get(this).connected == 1 {
            if let Some((peer_host, peer_port)) = ds_peer(this) {
                if peer_host != host || peer_port != port {
                    return Err(iae("Connected and packet address differ"));
                }
            }
        }
        let payload = java_byte_array_to_vec(ctx, data_arr, off, len)?;
        let target = format!("{host}:{port}");
        // Bracket the send in the GC-blocking protocol: it can park on a full
        // local socket buffer, same rationale as MulticastSocket's send in
        // native-io/src/net.rs — without this a cross-thread STW GC would
        // wait for the syscall to return instead of the thread reaching a
        // safepoint. No heap refs are read after the call, so a plain
        // begin/end pair (no ref re-sync) suffices.
        // A socket with room to send cannot park, so it skips the bracket (see
        // `FileDescriptorTable::udp_send_if_ready`).
        let send_result = match ctx
            .fd_table()
            .udp_send_if_ready(fd as u32, &payload, &target)
        {
            Ok(Some(n)) => Ok(n),
            Err(error) => Err(error),
            Ok(None) => {
                ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
                let send_result = ctx.fd_table().udp_send(fd as u32, &payload, &target);
                ctx.end_blocking_region();
                send_result
            }
        };
        send_result.map_err(|e| ioex(format!("UDP send: {e}")))?;
        Ok(None)
    }, NativeKind::SyntheticStub);

    r.register(
        ds,
        "receive",
        "(Ljava/net/DatagramPacket;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let pkt = obj_arg(args, 1)?;
            ds_require_not_closed(ctx, this)?;
            let fd = ds_require_open_socket(ctx, this)? as i32;
            let lay = dp_layout(ctx, pkt);
            let data_arr = match ctx.get_field(pkt, lay.buf) {
                Value::Object(Some(a)) => a,
                _ => return Err(ioex("DatagramPacket: null data")),
            };
            // JDK: fill `buf` from `offset`, for at most `bufLength` bytes.
            // Both are absent from the synthetic layout, where this degrades to
            // the whole array — today's behaviour.
            let arr_len = ctx.array_length(data_arr);
            let off = lay
                .offset
                .and_then(|s| ctx.get_field(pkt, s).as_int())
                .unwrap_or(0)
                .max(0) as usize;
            let room = arr_len.saturating_sub(off);
            let cap = match lay
                .buf_length
                .and_then(|s| ctx.get_field(pkt, s).as_int())
                .filter(|n| *n > 0)
            {
                Some(n) => room.min(n as usize),
                None => room,
            };
            let mut buf = vec![0u8; cap];
            let timeout_ms = ds_get(this).timeout;
            let d = if timeout_ms > 0 {
                Some(Duration::from_millis(timeout_ms as u64))
            } else {
                None
            };
            ctx.fd_table()
                .udp_set_read_timeout(fd as u32, d)
                .map_err(|e| ioex(format!("UDP timeout: {e}")))?;
            // GC-blocking audit: this recv parks in the OS for up to
            // soTimeout — or indefinitely when no timeout is set (same hang
            // mechanism documented on MulticastSocket's receive in
            // native-io/src/net.rs, which this DatagramSocket-keyed
            // registration duplicates for callers resolved via the
            // DatagramSocket-declaring class). Without the blocking-region
            // bracket, a cross-thread STW GC requested while this thread is
            // parked in udp_recv can never be satisfied: the thread isn't in
            // JIT code (can't be taken over) and isn't at a safepoint (can't
            // cooperate), so `pending` never reaches 0 and the takeover loop
            // spins until the external harness timeout. `pkt`/`data_arr` are
            // re-synced afterward in case a moving GC ran while parked.
            let mut blocked_refs = [Value::Object(Some(pkt)), Value::Object(Some(data_arr))];
            // A datagram that is already queued cannot park, so it is taken
            // outside the bracket (a TLAB retire, a root-snapshot deposit and a
            // post-block re-sync: ~45 us per call, see
            // `FileDescriptorTable::udp_recv_if_ready`); only an empty socket
            // reaches the bracketed wait below.
            let recv_result = match ctx.fd_table().udp_recv_if_ready(fd as u32, &mut buf) {
                Ok(Some(received)) => Ok(received),
                Err(error) => Err(error),
                Ok(None) => {
                    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
                    let recv_result = ctx.fd_table().udp_recv(fd as u32, &mut buf);
                    ctx.end_blocking_region_refs(&mut blocked_refs);
                    recv_result
                }
            };
            let pkt = match blocked_refs[0] {
                Value::Object(Some(o)) => o,
                _ => pkt,
            };
            let data_arr = match blocked_refs[1] {
                Value::Object(Some(o)) => o,
                _ => data_arr,
            };
            let (n, origin) = recv_result.map_err(udp_recv_ex)?;
            copy_bytes_into_java_array(ctx, data_arr, off as i32, &buf[..n])?;
            // Only `length` moves: `bufLength` is the buffer's capacity and
            // `offset` the caller's write position, and the JDK's
            // `setReceivedLength` leaves both alone.
            ctx.set_field(pkt, lay.length, Value::Int(n as i32));
            if let Some((oh, port)) = udp_origin_split(&origin) {
                // The datagram's origin, as numeric text off the wire.
                //
                // `alloc_inet_address_unnamed` ALLOCATES — and on a cold VM its
                // `populate_inet_holder` also loads and initialises
                // `InetAddress$InetAddressHolder`, which reliably triggers a
                // moving young collection (same hazard that function documents
                // for itself). `pkt` therefore cannot be held as a bare local
                // across it: the two writes below would land in a vacated
                // from-space copy of the packet and the caller's `getAddress()`
                // / `getPort()` would read the pre-receive values.
                let mut scope = NativeHandleScope::new(ctx);
                let pkt_h = scope.root(pkt);
                let ia = alloc_inet_address_unnamed(&mut *scope, &oh)?;
                let ia_h = scope.root(ia);
                let pkt_cur = scope.get(&pkt_h);
                let ia_cur = scope.get(&ia_h);
                scope.set_field(pkt_cur, lay.address, Value::Object(Some(ia_cur)));
                let pkt_cur = scope.get(&pkt_h);
                scope.set_field(pkt_cur, lay.port, Value::Int(port));            }
            Ok(None)
        }, NativeKind::SyntheticStub,
    );

    r.register(ds, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ds_get(this).fd;
        if fd >= 0 {
            // Ignore close errors — the fd may already be closed by
            // a racing caller; `closed` is still set unconditionally.
            let _ = ctx.fd_table().close(fd as u32);
        }
        ds_set(ctx.vm_identity(), this, |s| {
            s.closed = 1;
            s.fd = -1;        });
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "isClosed", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ds_get(this).closed)))
    }, NativeKind::SyntheticStub);
    // `getLocalPort()` is specified to answer `-1` once the socket is closed —
    // "the local port number to which this socket is bound, -1 if the socket
    // is closed". Keeping the port made `close()` look like a no-op to any
    // caller that reads the port back to decide whether to rebind.
    r.register(ds, "getLocalPort", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let sd = ds_get(this);
        Ok(Some(Value::Int(if sd.closed != 0 { -1 } else { sd.port })))
    }, NativeKind::SyntheticStub);
    // JDK order: the closed check FIRST, then the argument check. A closed
    // socket refuses `setSoTimeout(-1)` with `SocketException`, not with the
    // `IllegalArgumentException` an argument-first reading would give.
    r.register(ds, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ds_require_not_closed(ctx, this)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae("timeout < 0"));
        }
        ds_set(ctx.vm_identity(), this, |s| s.timeout = ms);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ds_require_not_closed(ctx, this)?;
        Ok(Some(Value::Int(ds_get(this).timeout)))
    }, NativeKind::SyntheticStub);

    // Same rationale as the `ServerSocket` setReuseAddress above: the synthetic
    // `DatagramSocket` has no real impl, so the JDK `setReuseAddress` bytecode
    // would NPE on uninitialised socket state. WildFly's `isPortAvailable` calls
    // `new DatagramSocket(port); setReuseAddress(true)` right after the
    // ServerSocket check. Push the option to the real UDP fd rather than
    // dropping it (this is the option multicast receivers need in order to
    // share a group port at all, so a silent no-op costs them every datagram),
    // and retain the requested value: `FileDescriptorTable` has no
    // `udp_reuse_address` read-back, so the getter has nothing else to answer
    // from — and answering a hardcoded `true` contradicted any caller who
    // turned it off.
    r.register(ds, "setReuseAddress", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let fd = ds_get(this).fd;
        if fd >= 0 {
            ctx.fd_table()
                .udp_set_reuse_address(fd as u32, on)
                .map_err(|e| ioex(format!("setReuseAddress failed: {e}")))?;
        }
        ds_set(ctx.vm_identity(), this, |s| s.reuse_address = i32::from(on));
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "getReuseAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sd = ds_get(this);
        // Ask the SOCKET when nobody called the setter. The historical answer
        // here was a hardcoded `1`, which HotSpot 25 contradicts on the same
        // host: `new DatagramSocket().getReuseAddress()` is **false** there.
        // `udp_reuse_address` did not exist when that constant was written.
        if sd.reuse_address < 0 && sd.fd >= 0 {
            if let Ok(on) = ctx.fd_table().udp_reuse_address(sd.fd as u32) {
                return Ok(Some(Value::Int(i32::from(on))));
            }
        }
        Ok(Some(Value::Int(if sd.reuse_address < 0 {
            1
        } else {
            sd.reuse_address
        })))
    }, NativeKind::SyntheticStub);
    // These three real-JDK declarations have bytecode that delegates through
    // a private DatagramSocket delegate object.  CratonVM's authoritative
    // socket state is the layout-independent DsSide table above, so force the
    // constructors and lifecycle operations through this same registration
    // instead of enabling phase-72's incompatible raw-slot duplicate.
    r.register(ds, "connect", "(Ljava/net/InetAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `java.net.DatagramSocket.connect(InetAddress, int)` is a null check
        // and then `connect(new InetSocketAddress(address, port))`, so BOTH
        // refusals below belong to it and neither depends on the socket being
        // open: the old body validated nothing and silently accepted a port
        // of -1 or 65536 by falling through its `(1..=65535)` guard.
        if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
            return Err(iae("Address can't be null"));
        }
        let requested_port = args.get(2).and_then(Value::as_int).unwrap_or(0);
        if !(0..=0xFFFF).contains(&requested_port) {
            // `InetSocketAddress`'s own `checkPort`, which is where this
            // throw happens on HotSpot — hence the colon with no space.
            return Err(iae(format!("port out of range:{requested_port}")));
        }
        let fd = ds_get(this).fd;
        if fd >= 0 {
            let host = match args.get(1) {
                Some(Value::Object(Some(address))) => match ctx.invoke_virtual(
                    *address,
                    "getHostAddress",
                    "()Ljava/lang/String;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(value)))) => {
                        ctx.read_string(value).unwrap_or_default()                    }
                    _ => String::new(),                },
                _ => String::new(),
            };
            let port = args.get(2).and_then(Value::as_int).unwrap_or(0);
            if !host.is_empty() && (1..=65535).contains(&port) {
                // DatagramSocket.connect records asynchronous failures for a
                // later I/O operation; it must not throw from the connect call.
                let _ = ctx
                    .fd_table()
                    .udp_connect(fd as u32, &format!("{host}:{port}"));
                // `isConnected()` reads this. It is set on the Java-side
                // transition rather than from the syscall result for the same
                // reason `connect` does not throw: the JDK reports a connect
                // failure at the next I/O operation, and `isConnected()` is
                // specified to keep answering true even after the socket is
                // closed.
                ds_set(ctx.vm_identity(), this, |sd| sd.connected = 1);
                ds_set_peer(ctx.vm_identity(), this, &host, port);            }        }
        Ok(None)
    }, NativeKind::SyntheticStub);
    // `disconnect()` is specified NOT to throw: "if the socket was not
    // connected, then this method has no effect". The kernel disassociation
    // goes through `FdTable::udp_disconnect` (POSIX `connect(AF_UNSPEC)`),
    // whose error is deliberately swallowed for the same reason.
    r.register(ds, "disconnect", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ds_get(this).fd;
        if fd >= 0 {
            let _ = _ctx.fd_table().udp_disconnect(fd as u32);
        }
        ds_set(_ctx.vm_identity(), this, |sd| sd.connected = 0);
        ds_clear_peer(this);
        Ok(None)
    }, NativeKind::SyntheticStub);
    // `isConnected()` had no registration at all, so it reached the abstract
    // `java.net.DatagramSocket` declaration and threw `InternalError` — right
    // after a `connect()` that had genuinely succeeded.
    r.register(ds, "isConnected", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ds_get(this).connected)))
    }, NativeKind::SyntheticStub);

    // `getPort()` / `getInetAddress()` — the read-back half of `connect`, and
    // the same missing-registration shape as `isConnected()` above.
    //
    // In a real-JDK build `java.net.DatagramSocket.getPort()` is
    // `delegate().getPort()`, and `delegate()` throws
    // `InternalError("Should not get here")` on a null `delegate` — which is
    // every socket this registrar constructs, because its `<init>` intercepts
    // never run the JDK constructor that would set one. So the answer was not
    // "wrong port", it was a hard InternalError immediately after a `connect()`
    // that had worked and an `isConnected()` that said so.
    //
    // `-1` / `null` for an unconnected socket is the JDK's specified answer.
    r.register(ds, "getPort", "()I", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = match ds_peer(this) {
            Some((_, port)) => port,
            None => -1,        };
        Ok(Some(Value::Int(port)))
    }, NativeKind::SyntheticStub);
    r.register(
        ds,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `ds_peer` releases the table lock before this allocates.
            let host = match ds_peer(this) {
                Some((host, _)) if !host.is_empty() => host,
                _ => return Ok(Some(Value::Object(None))),
            };
            let ia = alloc_inet_address_unnamed(ctx, &host)?;
            Ok(Some(Value::Object(Some(ia))))
        }, NativeKind::SyntheticStub,
    );

    // ── The rest of the delegate-backed surface ────────────────────────────
    //
    // Everything below was UNREGISTERED, and on a real-JDK build that is not
    // "missing" in a way anyone can see from the outside: `java.net
    // .DatagramSocket`'s own bytecode for each of these is `delegate().x()`,
    // and `delegate()` is
    //
    //     if (delegate == null) throw new InternalError("Should not get here");
    //
    // Every socket this registrar builds has a null `delegate` — its `<init>`
    // intercepts replace the JDK constructor that would set one — so each call
    // was a hard `InternalError`, not a wrong answer and not an
    // `UnsupportedOperationException`. Censused against `javap -p -s
    // java.net.DatagramSocket` and HotSpot 25 on the same host, one call per
    // declared method (`probes/DsCensus.java`): twelve of them threw.
    //
    // netty's `DnsNameResolverTest.testAddressAlreadyInUse` is the one that
    // filed it — it does `new DatagramSocket().getLocalSocketAddress()` — but
    // the census is what says which siblings share the hole.

    // `getLocalSocketAddress()` — null once closed, per the JDK.
    r.register(
        ds,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sd = ds_get(this);
            if sd.fd < 0 {
                return Ok(Some(Value::Object(None)));
            }
            let Ok(local) = ctx.fd_table().udp_local_addr(sd.fd as u32) else {
                return Ok(Some(Value::Object(None)));
            };
            let Some((host, port)) = udp_origin_split(&local) else {
                return Ok(Some(Value::Object(None)));
            };
            let isa = alloc_inet_socket_address_resolved(ctx, "", &host, port)?;
            Ok(Some(Value::Object(Some(isa))))
        }, NativeKind::SyntheticStub,
    );

    // `getRemoteSocketAddress()` — the `getInetAddress()`/`getPort()` pair as
    // one object, and null when unconnected. Answered from the peer table for
    // the same reason those two are: the JDK keeps answering after close.
    r.register(
        ds,
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (host, port) = match ds_peer(this) {
                Some((host, port)) if !host.is_empty() => (host, port),
                _ => return Ok(Some(Value::Object(None))),
            };
            let isa = alloc_inet_socket_address_resolved(ctx, "", &host, port)?;
            Ok(Some(Value::Object(Some(isa))))
        }, NativeKind::SyntheticStub,
    );

    // `bind(SocketAddress)` — a no-op before this, so `new DatagramSocket(null)`
    // (the JDK's unbound form) could never be bound at all: `isBound()` stayed
    // false and `getLocalSocketAddress()` stayed null on a socket the caller
    // had just bound. A null argument is the JDK's "ephemeral wildcard".
    r.register(ds, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        net_release_deferred(ctx);
        let spec = match args.get(1) {
            Some(Value::Object(Some(sa))) => {
                let (host, port) = read_socket_address_numeric(&*ctx, *sa)?;
                format!("{host}:{port}")
            }
            _ => "0.0.0.0:0".to_string(),
        };
        let sd = ds_get(this);
        ds_require_not_closed(ctx, this)?;
        // "if the socket is already bound" -> SocketException("Already
        // bound"). Every constructor here except `<init>(null)` opens and
        // binds an fd, so `fd >= 0` IS bound; rebinding it silently moved a
        // live socket to a new port, which `isBound()` had already reported
        // true for.
        if sd.fd >= 0 {
            return Err(socket_ex(ctx, "Already bound"));
        }
        // Same wildcard rule as the constructors: keep the dual stack.
        //
        // There is no rebind arm any more. There used to be one, for
        // `sd.fd >= 0`, and it is exactly the case the refusal above now
        // covers — an already-bound socket. A `udp_rebind` there was the JDK
        // contract inverted: it moved a live socket to another port and
        // reported success.
        let wildcard_port = cratonvm_native_api::fd_table::wildcard_bind_port(&spec);
        let fd = {
            let opened = match wildcard_port {
                Some(port) => {
                    crate::capability_gate::open_udp_wildcard_dual_stack_gated(&*ctx, port)
                }
                None => crate::capability_gate::open_udp_gated(&*ctx, Some(&spec)),
            };
            // Keep the JDK type: an unavailable address is `BindException`,
            // not a bare `IOException`. Same reasoning as
            // `capability_gate::translate_bind_io`, which serves the
            // constructor paths.
            let fd = opened.map_err(|e| {
                crate::capability_gate::translate_open_failure(e, |io| {
                    format!("DatagramSocket.bind: {io}")
                })
            })?;
            fd as i32
        };
        let port = ctx
            .fd_table()
            .udp_local_addr(fd as u32)
            .ok()
            .and_then(|s| udp_origin_split(&s).map(|(_, p)| p))
            .unwrap_or(0);
        ds_set(ctx.vm_identity(), this, |s| {
            s.fd = fd;
            s.port = port;
            s.closed = 0;        });
        Ok(None)
    }, NativeKind::SyntheticStub);

    // `<init>(SocketAddress)` — with a NULL argument this is the JDK's way to
    // ask for an UNBOUND socket, which is why it cannot share the `(int)`
    // ctor: that one always binds. Unregistered, it ran the real JDK
    // constructor, which left the side table empty — so the object reported
    // `getLocalSocketAddress() == null` while `send`/`receive` failed with
    // "closed" on a socket that had really been created.
    r.register(ds, "<init>", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        net_release_deferred(ctx);
        let Some(Value::Object(Some(sa))) = args.get(1) else {
            // Unbound: no fd yet. `bind()` above supplies one.
            ds_set(ctx.vm_identity(), this, |s| {
                s.fd = -1;
                s.port = 0;
                s.closed = 0;
                s.timeout = 0;            });
            return Ok(None);
        };
        let (host, port) = read_socket_address_numeric(&*ctx, *sa)?;
        let opened = if host == "0.0.0.0" || host == "[::]" {
            crate::capability_gate::open_udp_wildcard_dual_stack_gated(
                &*ctx,
                port.clamp(0, 65535) as u16,
            )
        } else {
            crate::capability_gate::open_udp_gated(&*ctx, Some(&format!("{host}:{port}")))
        };
        let fd = opened.map_err(|e| {
            crate::capability_gate::translate_open_failure(e, |io| format!("UDP bind: {io}"))
        })?;
        let actual_port = ctx
            .fd_table()
            .udp_local_addr(fd)
            .ok()
            .and_then(|s| udp_origin_split(&s).map(|(_, p)| p))
            .unwrap_or(port);
        ds_socket_defaults(&*ctx, fd);
        ds_set(ctx.vm_identity(), this, |s| {
            s.port = actual_port;
            s.closed = 0;
            s.timeout = 0;
            s.fd = fd as i32;        });
        Ok(None)
    }, NativeKind::SyntheticStub);

    // The four buffer-size / traffic-class accessors, and `supportedOptions`.
    // Same `delegate()` InternalError as the rest; each one has a
    // `FileDescriptorTable` primitive already.
    r.register(ds, "getSendBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ds_require_open_socket(ctx, this)?;
        let n = ctx
            .fd_table()
            .udp_send_buffer_size(fd)
            .map_err(|e| ioex(format!("getSendBufferSize: {e}")))?;
        Ok(Some(Value::Int(n as i32)))
    }, NativeKind::SyntheticStub);
    // The four size/class setters carry the JDK's own wording. "Invalid send
    // size" is not a paraphrase of "negative send buffer size": zero is
    // refused too, which the old text denied.
    r.register(ds, "setSendBufferSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if n <= 0 {
            return Err(iae("Invalid send size"));
        }
        let fd = ds_require_open_socket(ctx, this)?;
        ctx.fd_table()
            .udp_set_send_buffer_size(fd, n as usize)
            .map_err(|e| ioex(format!("setSendBufferSize: {e}")))?;
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "getReceiveBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ds_require_open_socket(ctx, this)?;
        let n = ctx
            .fd_table()
            .udp_recv_buffer_size(fd)
            .map_err(|e| ioex(format!("getReceiveBufferSize: {e}")))?;
        Ok(Some(Value::Int(n as i32)))
    }, NativeKind::SyntheticStub);
    r.register(ds, "setReceiveBufferSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if n <= 0 {
            return Err(iae("Invalid receive size"));
        }
        let fd = ds_require_open_socket(ctx, this)?;
        ctx.fd_table()
            .udp_set_recv_buffer_size(fd, n as usize)
            .map_err(|e| ioex(format!("setReceiveBufferSize: {e}")))?;
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ds, "getTrafficClass", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ds_require_open_socket(ctx, this)?;
        let n = ctx
            .fd_table()
            .udp_tos(fd)
            .map_err(|e| ioex(format!("getTrafficClass: {e}")))?;
        Ok(Some(Value::Int(n as i32)))
    }, NativeKind::SyntheticStub);
    r.register(ds, "setTrafficClass", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if !(0..=255).contains(&n) {
            return Err(iae("Invalid IP_TOS value"));
        }
        let fd = ds_require_open_socket(ctx, this)?;
        ctx.fd_table()
            .udp_set_tos(fd, n as u32)
            .map_err(|e| ioex(format!("setTrafficClass: {e}")))?;
        Ok(None)
    }, NativeKind::SyntheticStub);

    // `supportedOptions()` — exactly the names the `setOption`/`getOption`
    // arms above answer, so the set cannot claim an option the pair refuses.
    r.register(ds, "supportedOptions", "()Ljava/util/Set;", |ctx, args| {
        let _this = obj_arg(args, 0)?;
        ds_supported_options(ctx)
    }, NativeKind::SyntheticStub);
}

/// The fd of an open `DatagramSocket`, or the JDK's "Socket is closed".
pub fn ds_require_open(this: ObjectRef) -> Result<u32, cratonvm_types::error::MethodCallFailed> {
    let fd = ds_get(this).fd;
    if fd < 0 {
        return Err(ioex("Socket is closed"));
    }
    Ok(fd as u32)
}

/// `if (isClosed()) throw new SocketException("Socket is closed")` — the first
/// line of nearly every `java.net.DatagramSocket` accessor.
///
/// The concrete type is the point. `java.io.IOException` walks straight past
/// `catch (SocketException e)`, and the JDK's own bodies throw the subclass,
/// so a caller that distinguishes "this socket is gone" from "this send
/// failed" cannot do so against an `ioex`. Closed is `closed != 0` alone: a
/// socket built with `new DatagramSocket(null)` has no fd yet and is UNBOUND,
/// not closed, and must still answer its getters.
pub fn ds_require_not_closed(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    if ds_get(this).closed != 0 {
        return Err(socket_ex(ctx, "Socket is closed"));
    }
    Ok(())
}

/// [`ds_require_open`] with the JDK's exception TYPE.
///
/// Same body, `java.net.SocketException` instead of `java.io.IOException`;
/// see [`ds_require_not_closed`] for why that matters.
pub fn ds_require_open_socket(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
) -> Result<u32, cratonvm_types::error::MethodCallFailed> {
    let fd = ds_get(this).fd;
    if fd < 0 {
        return Err(socket_ex(ctx, "Socket is closed"));
    }
    Ok(fd as u32)}

/// Build the `Set<SocketOption<?>>` for `DatagramSocket.supportedOptions()`.
///
/// Each element is the real JDK constant, read out of its declaring class by
/// name, so the identity a caller compares against
/// (`supportedOptions().contains(SO_RCVBUF)`) is the same object it holds — a
/// freshly minted look-alike would not be equal. Two declaring classes:
/// `java.net.StandardSocketOptions` and, for `IP_DONTFRAGMENT`,
/// `jdk.net.ExtendedSocketOptions`.
pub fn ds_supported_options(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let created = ctx.new_object_initialized("java/util/LinkedHashSet", "()V", &[])?;
    let Some(Value::Object(Some(set))) = created else {
        return Ok(Some(Value::Object(None)));
    };
    let pin = ctx.pin_native_root(set);
    // INITIALIZE, do not merely resolve: `class_id_by_name` answers "loaded",
    // and every one of these constants is assigned in `<clinit>`. Reading them
    // off a resolved-but-uninitialised class returns eight nulls and the set
    // comes back empty.
    // `IP_DONTFRAGMENT` is an EXTENDED option and lives in a different class,
    // which is exactly why it was the one missing from the first version of
    // this set: the `setOption`/`getOption` arms above answer it, so leaving
    // it out broke this function's own invariant — that the advertised set is
    // the set the pair serves — in the under-advertising direction, the one a
    // one-way ratchet cannot see. HotSpot 25 lists it too (9 options, not 8).
    for (class_name, fields) in [
        (
            "java/net/StandardSocketOptions",
            &[
                "SO_SNDBUF",
                "SO_RCVBUF",
                "SO_REUSEADDR",
                "SO_BROADCAST",
                "IP_TOS",
                "IP_MULTICAST_IF",
                "IP_MULTICAST_TTL",
                "IP_MULTICAST_LOOP",
            ][..],
        ),
        ("jdk/net/ExtendedSocketOptions", &["IP_DONTFRAGMENT"][..]),
    ] {
        // INITIALIZE, do not merely resolve — see above.
        let Ok(cid) = ctx.ensure_class_initialized(class_name) else {
            continue;
        };
        for field in fields {
            let Some(idx) = ctx.static_field_index_by_name(cid, field) else {
                continue;
            };
            let value = ctx.get_static_field(cid, idx);
            if let Value::Object(Some(_)) = value {
                let set_cur = ctx.read_native_pin(pin, set);
                let _ = ctx.invoke_virtual(set_cur, "add", "(Ljava/lang/Object;)Z", &[value]);
            }
        }
    }
    let set_cur = ctx.read_native_pin(pin, set);
    ctx.unpin_native_roots(pin);
    Ok(Some(Value::Object(Some(set_cur))))
}

// ===========================================================================
// RE.8 — java.net.NetworkInterface
// ===========================================================================

pub fn re8_enumerate_local_ips() -> Vec<IpAddr> {
    use std::net::UdpSocket;
    let mut out = vec![IpAddr::V4(Ipv4Addr::LOCALHOST)];
    if let Ok(s) = UdpSocket::bind("0.0.0.0:0") {
        if s.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                if !addr.ip().is_loopback() {
                    out.push(addr.ip());
                }
            }
        }
    }
    if let Ok(s) = UdpSocket::bind("[::]:0") {
        if s.connect("[2001:4860:4860::8888]:80").is_ok() {
            if let Ok(addr) = s.local_addr() {
                if !addr.ip().is_loopback() && !out.contains(&addr.ip()) {
                    out.push(addr.ip());
                }
            }
        }
    }
    let lookup = format!("{}:0", hostname_string());
    if let Ok(addrs) = std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
        for sa in addrs {
            if !out.contains(&sa.ip()) {
                out.push(sa.ip());
            }
        }
    }
    // Finally the addresses the kernel actually has configured. The three
    // probes above only find the primary outbound address and whatever DNS
    // says about our own name, so an address on a secondary NIC (or any
    // interface with no default route) was invisible — and `boundInetAddress0`
    // reads a miss as "that address is not mine" and rejects a bind that would
    // have worked. Appended, not prepended, so the outbound-probe ordering the
    // other callers see is unchanged.
    for iface in re8_scan_host_ifaces() {
        for ip in iface.addrs {
            if !out.contains(&ip) {
                out.push(ip);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// RE.8 — host interface enumeration
//
// Everything `java.net.NetworkInterface` reports comes from here. Until wave 4
// this module modelled exactly ONE interface (loopback), so `isUp0`/`isP2P0`/
// `supportsMulticast0`/`getMTU0`/`getMacAddr0` were all "loopback's answers"
// and `getAll()` handed out a single carrier — which is why HotSpot reported
// several interfaces and a real MAC where CratonVM reported one and none.
// ---------------------------------------------------------------------------

/// One host network interface, as the OS reports it.
pub struct Re8HostIface {
    pub name: String,
    /// The `NetworkInterface.getDisplayName()` text when it differs from `name`
    /// (Windows: the adapter description). `None` means "same as `name`".
    pub display_name: Option<String>,
    /// Kernel interface index (`if_nametoindex`), 0 when unknown.
    pub index: i32,
    /// `IFF_*` flag word.
    pub flags: u32,
    /// `None` when the platform publishes no per-interface MTU.
    pub mtu: Option<i32>,
    /// Hardware address; empty for loopback and other address-less interfaces.
    pub mac: Vec<u8>,
    pub addrs: Vec<IpAddr>,
    /// The IPv6 scope each entry of `addrs` carries, same length and order
    /// (0 for IPv4, and for an IPv6 address that has no scope).
    ///
    /// This column exists because the two platforms genuinely disagree, and a
    /// single rule gets one of them wrong. On Linux the JDK's enumeration
    /// reads `/proc/net/if_inet6` and stores the interface INDEX as the scope
    /// of every row, so `lo`'s address really is `0:0:0:0:0:0:0:1%lo` with
    /// `getScopeId() == 1` even though `getifaddrs` reports
    /// `sin6_scope_id == 0` for it. On Windows the JDK reads
    /// `sin6_scope_id` from `GetAdaptersAddresses`, which is the interface
    /// index for a link-local address and **0 for the loopback and for global
    /// addresses** — measured against HotSpot 25 on this host, where
    /// `loopback_0`'s address is a bare `0:0:0:0:0:0:0:1` with
    /// `getScopeId() == 0` and `getScopedInterface() == null`, and only the
    /// `fe80:` rows are scoped.
    ///
    /// Scoping everything, which is what this did, is right on Linux and
    /// wrong on Windows in a way that leaves the address text unable to
    /// round-trip: `getHostAddress()` answered
    /// `0:0:0:0:0:0:0:1%{3C307829-…}` and `InetAddress.getByName` of that
    /// threw `UnknownHostException`. netty's `Socks4ProxyHandler` puts
    /// `getHostAddress()` of the destination on the wire and the proxy server
    /// feeds the text straight back to `getByName`, so every proxy-chain hop
    /// over the IPv6 loopback failed — 8 of `ProxyHandlerTest`'s 47
    /// parameterisations, against 47/47 on HotSpot.
    pub addr_scopes: Vec<u32>,
}

// `IFF_*` bits. The low bits are identical on Linux and the BSDs; only
// IFF_MULTICAST moved (0x1000 on Linux, 0x8000 on BSD/macOS), so it is selected
// per target here rather than taken from `libc`, whose export set for these
// constants varies by platform.
pub const RE8_IFF_UP: u32 = 0x1;
pub const RE8_IFF_LOOPBACK: u32 = 0x8;
pub const RE8_IFF_POINTOPOINT: u32 = 0x10;
pub const RE8_IFF_RUNNING: u32 = 0x40;
#[cfg(any(target_os = "linux", target_os = "android"))]
pub const RE8_IFF_MULTICAST: u32 = 0x1000;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub const RE8_IFF_MULTICAST: u32 = 0x8000;

/// Read one `/sys/class/net/<iface>/<file>` attribute (Linux only).
///
/// The interface name is rejected if it could escape the directory, so a name
/// arriving from Java bytecode can never be used to read an arbitrary file.
pub fn re8_sys_attr(name: &str, file: &str) -> Option<String> {
    if name.is_empty() || name.starts_with('.') || name.contains('/') || name.contains('\\') {
        return None;
    }
    std::fs::read_to_string(format!("/sys/class/net/{name}/{file}"))
        .ok()
        .map(|text| text.trim().to_string())
}

/// Parse the kernel's `aa:bb:cc:dd:ee:ff` hardware-address rendering.
///
/// Returns an EMPTY vector for "this interface has no hardware address": the
/// kernel prints all-zero for loopback/tun, and a fabricated 00:00:00:00:00:00
/// is worse than `null` — it is a syntactically valid MAC, so UUID-v1
/// generators and cluster-member-id derivations accept it and every host ends
/// up with the same identity instead of taking the documented "unavailable"
/// fallback.
pub fn re8_parse_mac(text: &str) -> Vec<u8> {
    let fields = text.split(':').count();
    let bytes: Vec<u8> = text
        .split(':')
        .filter_map(|part| u8::from_str_radix(part, 16).ok())
        .collect();
    if bytes.len() != fields || bytes.len() < 6 || bytes.iter().all(|b| *b == 0) {
        return Vec::new();
    }
    bytes
}

#[cfg(unix)]
pub fn re8_iface_index(name: &str) -> i32 {
    if let Some(index) = re8_sys_attr(name, "ifindex").and_then(|t| t.parse::<i32>().ok()) {
        return index;
    }
    let Ok(cname) = std::ffi::CString::new(name) else {
        return 0;
    };
    // SAFETY: `cname` is a NUL-terminated C string that outlives the call.
    (unsafe { libc::if_nametoindex(cname.as_ptr()) }) as i32
}

#[cfg(not(unix))]
pub fn re8_iface_index(_name: &str) -> i32 {
    0
}

/// Enumerate the host's real network interfaces.
///
/// Unix: `getifaddrs(3)` supplies the interface list, per-interface addresses
/// and the `IFF_*` flags in one pass; Linux additionally publishes the MTU and
/// the hardware address as plain files under `/sys/class/net`, so those are
/// read from there rather than by decoding `AF_PACKET` link-layer sockaddrs.
///
/// Returns an EMPTY vector when the host cannot be enumerated — the callers
/// then fall back to the loopback-only model this module has always used.
#[cfg(unix)]
pub fn re8_scan_host_ifaces() -> Vec<Re8HostIface> {
    let mut out: Vec<Re8HostIface> = Vec::new();
    let mut head: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `head` is a valid out-parameter; on success the kernel-allocated
    // list is freed by the `freeifaddrs` below and never escapes this function.
    if unsafe { libc::getifaddrs(&mut head) } != 0 || head.is_null() {
        return out;
    }
    let mut cursor = head;
    while !cursor.is_null() {
        // SAFETY: the list is walked to its null terminator while we still own
        // it (`freeifaddrs` runs after the loop).
        let entry = unsafe { &*cursor };
        cursor = entry.ifa_next;
        if entry.ifa_name.is_null() {
            continue;
        }
        // SAFETY: `ifa_name` is a NUL-terminated C string owned by the list.
        let name = unsafe { std::ffi::CStr::from_ptr(entry.ifa_name) }
            .to_string_lossy()
            .into_owned();
        let flags = entry.ifa_flags as u32;
        // An interface with no IP address still exists (an unconfigured NIC, or
        // the AF_PACKET/AF_LINK entry the kernel emits per device), and real
        // `getAll()` reports it — so record the name whatever the family is and
        // only push an address for the two families we can decode.
        let ip = if entry.ifa_addr.is_null() {
            None
        } else {
            // SAFETY: `ifa_addr` points at a `sockaddr` whose `sa_family` tag
            // selects the concrete layout read below; both are list-owned.
            let family = unsafe { (*entry.ifa_addr).sa_family } as i32;
            if family == libc::AF_INET {
                let sin = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in) };
                Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(
                    sin.sin_addr.s_addr,
                ))))
            } else if family == libc::AF_INET6 {
                let sin6 = unsafe { &*(entry.ifa_addr as *const libc::sockaddr_in6) };
                Some(IpAddr::V6(Ipv6Addr::from(sin6.sin6_addr.s6_addr)))
            } else {
                None
            }
        };
        // The kernel emits one entry per (interface, address), so fold repeats
        // of a name together instead of reporting the same NIC several times.
        let seen = out.iter().position(|existing| existing.name == name);
        let slot = match seen {
            Some(index) => index,
            None => {
                out.push(Re8HostIface {
                    name,
                    display_name: None,
                    index: 0,
                    flags: 0,
                    mtu: None,
                    mac: Vec::new(),
                    addrs: Vec::new(),
                    addr_scopes: Vec::new(),
                });
                out.len() - 1
            }
        };
        let existing = &mut out[slot];
        existing.flags |= flags;
        if let Some(ip) = ip {
            if !existing.addrs.contains(&ip) {
                existing.addrs.push(ip);
            }
        }
    }
    // SAFETY: `head` came from the successful `getifaddrs` above, is still the
    // list head (the walk advanced a copy), and is freed exactly once.
    unsafe { libc::freeifaddrs(head) };
    // The JDK's Linux enumeration CREATES a `NetworkInterface` from an
    // address, so an interface carrying none is not in `getAll0()`'s output at
    // all: HotSpot JDK 25 on this host reports five interfaces where the
    // `AF_PACKET` entries `getifaddrs` also returns would make six (`ens1` is
    // configured but address-less). Reporting the extra one gave
    // `getNetworkInterfaces()` a member `getByName` on HotSpot answers `null`
    // for, and put an interface with an empty `getInetAddresses()` in front of
    // any "pick an interface and take its address" scan.
    out.retain(|iface| !iface.addrs.is_empty());
    // The JDK builds its interface list by PREPENDING each newly seen
    // interface, so `getNetworkInterfaces()` hands them back in the reverse of
    // the order it enumerated them, and `lo` — first out of the kernel — comes
    // last. Reversing matches that shape, and on this host it makes the
    // IPv4-carrying tail (`docker0`, `eth0`, `lo`) identical to HotSpot's.
    //
    // It does NOT reproduce HotSpot's order exactly, and cannot: the JDK's
    // Linux IPv6 pass reads `/proc/net/if_inet6`, whose row order is a kernel
    // hash-table walk, not the ascending-index order `getifaddrs` returns.
    // With 14 veth interfaces present, HotSpot's block was
    // 3422, 2845, 3427, 3424, … and ours is strictly descending. `Enumeration`
    // order is unspecified by the API, so this is a difference rather than a
    // defect — but do not read a probe row that names "the first non-loopback
    // interface" as a divergence: it is naming whichever one came first.
    out.reverse();
    for iface in &mut out {
        iface.index = re8_iface_index(&iface.name);
        iface.mtu = re8_sys_attr(&iface.name, "mtu").and_then(|t| t.parse::<i32>().ok());
        iface.mac = re8_sys_attr(&iface.name, "address")
            .map(|t| re8_parse_mac(&t))
            .unwrap_or_default();
        // Unchanged behaviour, now written down as data: the JDK's Linux pass
        // scopes every IPv6 row to the interface index (see `addr_scopes`).
        // `.max(1)` keeps a scope even when `if_nametoindex` could not answer,
        // which is what the unconditional suffix used to do.
        let scope = (iface.index.max(1)) as u32;
        iface.addr_scopes = iface
            .addrs
            .iter()
            .map(|ip| if ip.is_ipv6() { scope } else { 0 })
            .collect();
    }
    out
}

/// Windows uses `GetAdaptersAddresses`, the same IP Helper API family used by
/// the JDK's Windows network-interface implementation. Keep the raw FFI
/// layout local: this crate deliberately has no Windows-only dependency.
#[cfg(windows)]
#[repr(C)]
pub struct Re8WinAdapterAddress {
    pub length: u32,
    pub if_index: u32,
    pub next: *mut Re8WinAdapterAddress,
    pub adapter_name: *const std::ffi::c_char,
    pub first_unicast_address: *mut Re8WinUnicastAddress,
    pub first_anycast_address: *mut core::ffi::c_void,
    pub first_multicast_address: *mut core::ffi::c_void,
    pub first_dns_server_address: *mut core::ffi::c_void,
    pub dns_suffix: *const u16,
    pub description: *const u16,
    pub friendly_name: *const u16,
    pub physical_address: [u8; 8],
    pub physical_address_length: u32,
    pub flags: u32,
    pub mtu: u32,
    pub interface_type: u32,
    pub oper_status: u32,
    pub ipv6_if_index: u32,
    pub zone_indices: [u32; 16],
    // Vista+ `IP_ADAPTER_ADDRESSES_LH` tail, read for `Luid` only.
    pub first_prefix: *mut core::ffi::c_void,
    pub transmit_link_speed: u64,
    pub receive_link_speed: u64,
    pub first_wins_server_address: *mut core::ffi::c_void,
    pub first_gateway_address: *mut core::ffi::c_void,
    pub ipv4_metric: u32,
    pub ipv6_metric: u32,
    pub luid: u64,
}
#[cfg(windows)]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Re8WinSocketAddress {
    pub address: *const u8,
    pub length: i32,
}

#[cfg(windows)]
#[repr(C)]
pub struct Re8WinUnicastAddress {
    pub length: u32,
    pub flags: u32,
    pub next: *mut Re8WinUnicastAddress,
    pub address: Re8WinSocketAddress,
}

#[cfg(windows)]
/// Takes the `SOCKET_ADDRESS` BY REFERENCE.
///
/// It was by value, which does not compile on Windows at all:
/// `Re8WinSocketAddress` is not `Copy`, and the caller reads it out of a
/// `&IP_ADAPTER_UNICAST_ADDRESS` borrowed from the OS-owned list. The whole
/// Windows arm of this function is `#[cfg(windows)]`, so a Linux-only build
/// never type-checked it.
pub unsafe fn re8_win_ip(address: &Re8WinSocketAddress) -> Option<(IpAddr, u32)> {
    if address.address.is_null() || address.length < 2 {
        return None;
    }
    let family = u16::from_ne_bytes([*address.address, *address.address.add(1)]);
    match family {
        2 if address.length >= 8 => Some((
            IpAddr::V4(Ipv4Addr::new(
                *address.address.add(4),
                *address.address.add(5),
                *address.address.add(6),
                *address.address.add(7),
            )),
            0,
        )),
        23 if address.length >= 24 => {
            let mut octets = [0u8; 16];
            std::ptr::copy_nonoverlapping(address.address.add(8), octets.as_mut_ptr(), 16);
            // `sockaddr_in6` is family(2) port(2) flowinfo(4) addr(16)
            // scope_id(4); a short `SOCKET_ADDRESS` that stops before the
            // scope word is read as unscoped rather than as garbage.
            let scope = if address.length >= 28 {
                let mut raw = [0u8; 4];
                std::ptr::copy_nonoverlapping(address.address.add(24), raw.as_mut_ptr(), 4);
                u32::from_ne_bytes(raw)
            } else {
                0
            };
            Some((IpAddr::V6(Ipv6Addr::from(octets)), scope))
        }
        _ => None,
    }
}

#[cfg(windows)]
#[link(name = "Iphlpapi")]
extern "system" {
    fn GetAdaptersAddresses(
        family: u32,
        flags: u32,
        reserved: *mut core::ffi::c_void,
        addresses: *mut Re8WinAdapterAddress,
        size: *mut u32,
    ) -> u32;
}

/// `NetworkInterface.getName()` for a Windows adapter, the way the JDK's
/// `libnet` builds it: a type word and the `NET_LUID` index
/// (`NET_LUID_LH.Info.NetLuidIndex`, bits 24..48 of the 64-bit LUID).
#[cfg(windows)]
pub fn re8_win_interface_name(interface_type: u32, luid: u64) -> String {
    let word = match interface_type {
        6 => "ethernet",
        9 => "tokenring",
        23 => "ppp",
        24 => "loopback",
        37 => "atm",
        71 => "wireless",
        131 => "tunnel",
        144 => "firewire",
        _ => "net",
    };
    format!("{}_{}", word, (luid >> 24) & 0x00FF_FFFF)
}

/// A NUL-terminated UTF-16 string owned by the OS adapter list.
///
/// # Safety
/// `text` must be a valid NUL-terminated UTF-16 string.
#[cfg(windows)]
pub unsafe fn re8_win_wide_to_string(text: *const u16) -> String {
    let mut len = 0usize;
    while *text.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
}
#[cfg(windows)]
pub fn re8_scan_host_ifaces() -> Vec<Re8HostIface> {
    let mut needed = 15_000u32;
    for _ in 0..3 {
        let words =
            (needed as usize + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
        let mut storage = vec![std::mem::MaybeUninit::<usize>::uninit(); words.max(1)];
        let mut size = (storage.len() * std::mem::size_of::<usize>()) as u32;
        let result = unsafe {
            GetAdaptersAddresses(
                0,
                0x110,
                std::ptr::null_mut(),
                storage.as_mut_ptr() as *mut Re8WinAdapterAddress,
                &mut size,
            )
        };
        if result == 111 {
            needed = size.max(needed.saturating_mul(2));
            continue;
        }
        if result != 0 {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut current = storage.as_mut_ptr() as *mut Re8WinAdapterAddress;
        while !current.is_null() {
            let adapter = unsafe { &*current };
            let adapter_guid = if adapter.adapter_name.is_null() {
                String::new()
            } else {
                unsafe { std::ffi::CStr::from_ptr(adapter.adapter_name) }
                    .to_string_lossy()
                    .into_owned()
            };
            // HotSpot's Windows `NetworkInterface` names an adapter
            // `<type>_<NetLuidIndex>` (`ethernet_32769`, `wireless_32768`,
            // `loopback_0`, `ppp_32768`, `tunnel_32512`), not the adapter GUID, and
            // shows the adapter description as the display name. The GUID spelling
            // is not even a legal URI scope id: `http://[fe80::1%{CE4C..}]:80/`
            // fails `new URI(..)`, which is what `TestStartupIPv6Connectors` does
            // with a link-local address's `getHostAddress()`.
            let name = if adapter_guid.is_empty() {
                String::new()
            } else {
                re8_win_interface_name(adapter.interface_type, adapter.luid)            };
            let display_name = (!adapter.description.is_null())
                .then(|| unsafe { re8_win_wide_to_string(adapter.description) })
                .filter(|text| !text.is_empty());
            if !name.is_empty() {
                let mut flags = 0;
                if adapter.oper_status == 1 {
                    flags |= RE8_IFF_UP | RE8_IFF_RUNNING;
                }
                if adapter.interface_type == 24 {
                    flags |= RE8_IFF_LOOPBACK;
                }
                if adapter.interface_type == 23 {
                    flags |= RE8_IFF_POINTOPOINT;
                }
                if adapter.flags & 0x20 == 0 {
                    flags |= RE8_IFF_MULTICAST;
                }
                let mac_len =
                    (adapter.physical_address_length as usize).min(adapter.physical_address.len());
                let mac = if mac_len >= 6
                    && !adapter.physical_address[..mac_len]
                        .iter()
                        .all(|byte| *byte == 0)
                {
                    adapter.physical_address[..mac_len].to_vec()
                } else {
                    Vec::new()
                };
                let mut addrs = Vec::new();
                let mut addr_scopes = Vec::new();
                let mut unicast = adapter.first_unicast_address;
                while !unicast.is_null() {
                    let entry = unsafe { &*unicast };
                    if let Some((ip, scope)) = unsafe { re8_win_ip(&entry.address) } {
                        if !addrs.contains(&ip) {
                            addrs.push(ip);
                            addr_scopes.push(scope);
                        }
                    }
                    unicast = entry.next;
                }
                out.push(Re8HostIface {
                    name,
                    display_name,
                    index: if adapter.if_index != 0 {
                        adapter.if_index
                    } else {
                        adapter.ipv6_if_index
                    } as i32,
                    flags,
                    mtu: (adapter.mtu != 0).then_some(adapter.mtu.min(i32::MAX as u32) as i32),
                    mac,
                    addrs,
                    addr_scopes,
                });
            }
            current = adapter.next;
        }
        return out;
    }
    Vec::new()
}

#[cfg(all(not(unix), not(windows)))]
pub fn re8_scan_host_ifaces() -> Vec<Re8HostIface> {
    Vec::new()
}

/// Flags / MTU / hardware address for ONE named interface.
///
/// The `*0` natives are handed just `(name, index)`, and on Linux every
/// attribute they need is a plain file, so answer from `/sys` without paying
/// for a full `getifaddrs` walk per call. `None` means "this host cannot tell
/// us about that interface".
pub fn re8_host_iface_by_name(name: &str) -> Option<Re8HostIface> {
    if let Some(text) = re8_sys_attr(name, "flags") {
        let mut flags = text
            .strip_prefix("0x")
            .and_then(|hex| u32::from_str_radix(hex, 16).ok())
            .or_else(|| text.parse::<u32>().ok())?;
        // `/sys/class/net/<if>/flags` is the kernel's `dev->flags`, which
        // NEVER carries IFF_RUNNING (0x40) — that bit is synthesised by
        // `dev_get_flags()` for `SIOCGIFFLAGS`/`getifaddrs` from the device's
        // operational state. Measured on this host: `lo` reads 0x9 and `eth0`
        // reads 0x1003, so `isUp0`'s faithful `IFF_UP && IFF_RUNNING` test was
        // false for EVERY interface and `NetworkInterface.isUp()` answered
        // false across the board — where HotSpot answers true for all five.
        // Anything that picks "the first non-loopback interface that is up"
        // found nothing at all. Re-derive the bit the way the kernel does.
        if flags & RE8_IFF_UP != 0 {
            let oper = re8_sys_attr(name, "operstate").unwrap_or_default();
            // `netif_oper_up()`: IF_OPER_UP or IF_OPER_UNKNOWN. `lo` reports
            // "unknown" and is nonetheless running.
            if oper.is_empty() || oper == "up" || oper == "unknown" {
                flags |= RE8_IFF_RUNNING;
            }
        }
        return Some(Re8HostIface {
            name: name.to_string(),
            display_name: None,
            index: re8_iface_index(name),
            flags,
            mtu: re8_sys_attr(name, "mtu").and_then(|t| t.parse::<i32>().ok()),
            mac: re8_sys_attr(name, "address")
                .map(|t| re8_parse_mac(&t))
                .unwrap_or_default(),
            addrs: Vec::new(),
            addr_scopes: Vec::new(),
        });
    }
    re8_scan_host_ifaces()
        .into_iter()
        .find(|iface| iface.name == name)
}

/// Loopback's MTU, per platform — the fallback for a host we cannot query.
/// `lo` is 65536 on Linux, `lo0` is 16384 on macOS, and Windows loopback is
/// 1500; one flat 1500 was wrong on two of the three.
pub fn re8_loopback_mtu() -> i32 {
    if cfg!(target_os = "linux") {
        65536
    } else if cfg!(target_os = "macos") {
        16384
    } else {
        1500
    }
}

/// Read a `String` argument (the `*0` natives are all static, so index 0 is the
/// interface name, not a receiver).
pub fn re8_name_arg(ctx: &mut dyn NativeContext, args: &[Value], index: usize) -> String {
    match args.get(index).copied() {
        Some(Value::Object(Some(s))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Read the `name` field off a `java/net/NetworkInterface` receiver.
///
/// BY NAME, not by slot: the receivers this module hands out are REAL-layout
/// objects built through the JDK's own constructor, whose field order is not
/// the synthetic one (mixing the two is what produced the `arraylength null`
/// NPE in `NetworkInterface$1` documented at `register_re8_network_interface`).
pub fn re8_receiver_name(ctx: &mut dyn NativeContext, args: &[Value]) -> String {
    match args.first().copied() {
        Some(Value::Object(Some(this))) => match ctx.get_field_by_name(this, "name") {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

/// Materialise a hardware address as a Java `byte[]`, or null when the
/// interface has none (the value `getHardwareAddress` is specified to return
/// when the address does not exist or is not accessible).
pub fn re8_mac_array(ctx: &mut dyn NativeContext, mac: &[u8]) -> MethodCallResult {
    if mac.is_empty() {
        return Ok(Some(Value::Object(None)));
    }
    let arr = ctx.new_array(ArrayElementType::Byte, mac.len());
    for (i, byte) in mac.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*byte as i8 as i32));
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// The fallback interface, used only when the host cannot be enumerated (see
/// [`re8_scan_host_ifaces`]). `getAll`, `getByName0`, `getByIndex0` and
/// `getByInetAddress0` all key off the same source, so they cannot contradict
/// each other in either mode.
pub const RE8_LOOPBACK_NAME: &str = "lo";
pub const RE8_LOOPBACK_INDEX: i32 = 1;

/// Build one REAL-layout `java.net.NetworkInterface` for a host interface.
///
/// Same construction path as [`re8_make_loopback_interface`] — the
/// package-private `(String,int,InetAddress[])` constructor plus the `childs`
/// fix-up — so the real `getInetAddresses()`/`toString()`/`getSubInterfaces()`
/// bytecode reads the fields it expects. Returns `None` when a step fails; the
/// caller then skips this interface.
///
/// GC note: the returned reference is NOT pinned. A caller that allocates again
/// before using it must pin it across that allocation.
pub fn re8_make_interface(
    ctx: &mut dyn NativeContext,
    host: &Re8HostIface,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let addr_cid = ctx
        .class_id_by_name("java/net/InetAddress")
        .unwrap_or(ClassId::new(0));
    let mut addrs = ctx.new_ref_array(addr_cid, host.addrs.len());
    // First pin of the batch: `unpin_native_roots(base_pin)` at the end
    // releases this and every pin taken after it.
    let base_pin = ctx.pin_native_root(addrs);
    for (i, ip) in host.addrs.iter().enumerate() {
        // NO name. The JDK's `getAll0` builds these addresses from raw octets
        // and never supplies a hostName, so HotSpot prints `/127.0.0.1` and
        // `/fe80:…%eth0` for every row of `getInetAddresses()`. Labelling them
        // `localhost` / `<hostname>` — which this loop used to do — is the
        // exact divergence
        // `inetaddress-tostring-hostname-literal-addresses-FIXED.md`
        // removed everywhere else; this call site was missed. `getHostName()`
        // still answers the numeric text, because that is its no-name
        // fallback.
        let label = NO_HOST_NAME;
        // An IPv6 address gets the `%<interface>` suffix exactly when its
        // scope says so — which on Linux is every row and on Windows only the
        // link-local ones. `addr_scopes` carries the platform's answer and its
        // doc comment carries the measurement behind it; a single rule here
        // was right on one platform and unable to round-trip on the other.
        let scope = host.addr_scopes.get(i).copied().unwrap_or(0);
        let text = match ip {
            IpAddr::V6(_) if scope != 0 => format!("{ip}%{}", host.name),
            _ => ip.to_string(),
        };
        // Allocates several objects, so re-read the array through its pin
        // before storing into it (native stale-local family).
        let addr = alloc_inet_address(ctx, label, &text);
        addrs = ctx.read_native_pin(base_pin, addrs);
        ctx.set_array_element(addrs, i, Value::Object(Some(addr?)));
    }
    let name0 = ctx.create_string(&host.name);
    let name_pin = ctx.pin_native_root(name0);
    let iface0 = match ctx.new_object("java/net/NetworkInterface") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(base_pin);
            return Ok(None);
        }
    };
    let iface_pin = ctx.pin_native_root(iface0);
    let name = ctx.read_native_pin(name_pin, name0);
    let addrs = ctx.read_native_pin(base_pin, addrs);
    let iface = ctx.read_native_pin(iface_pin, iface0);
    let _ = ctx.invoke(
        "java/net/NetworkInterface",
        "<init>",
        "(Ljava/lang/String;I[Ljava/net/InetAddress;)V",
        &[
            Value::Object(Some(iface)),
            Value::Object(Some(name)),
            Value::Int(host.index),
            Value::Object(Some(addrs)),
        ],
    );
    // `Inet6Address.getScopedInterface()` is un-intercepted real-JDK bytecode
    // reading `holder6.scope_ifname`, and HotSpot's `createNetworkInterface`
    // sets it to the very interface being built. It can only be written HERE,
    // after the carrier exists: `alloc_inet_address` cannot mint one for its
    // own address (this function is what mints interfaces, and it allocates
    // addresses — the recursion has no base case). The rendered `%eth0` suffix
    // does not depend on this (the side table supplies it), but
    // `getScopedInterface()` answered null without it, and the JDK's own
    // `Inet6Address(String, byte[], NetworkInterface)` constructor reads
    // `getScopeId()` off these addresses to derive a scope for a new one.
    //
    // No allocation happens in this loop, so the pinned handles stay valid
    // throughout.
    let addrs_now = ctx.read_native_pin(base_pin, addrs);
    let iface_now = ctx.read_native_pin(iface_pin, iface0);
    for i in 0..ctx.array_length(addrs_now) {
        // Only a SCOPED address carries the interface. An unscoped one that
        // did got a `%<interface>` suffix out of `getHostAddress()` (the
        // renderer prefers `scope_ifname` over the numeric id) even though
        // nothing else about it was scoped — which is the bug this column
        // fixes, so the two must agree.
        let scope = host.addr_scopes.get(i as usize).copied().unwrap_or(0);
        if scope == 0 {
            continue;
        }
        if let Value::Object(Some(a)) = ctx.get_array_element(addrs_now, i) {
            if let Value::Object(Some(h6)) = ctx.get_field_by_name(a, "holder6") {
                ctx.set_field_by_name(h6, "scope_ifname", Value::Object(Some(iface_now)));
                // HotSpot answers the interface index from `getScopeId()` for
                // a scoped address; ours answered 0 for every one of them,
                // because nothing ever wrote the numeric half. The renderer
                // still prefers `scope_ifname`, so the text is unchanged.
                ctx.set_field_by_name(h6, "scope_id", Value::Int(scope as i32));
                ctx.set_field_by_name(h6, "scope_id_set", Value::Int(1));
            }
        }
    }
    // The package-private ctor leaves `childs` null and
    // `NetworkInterface.getSubInterfaces()`'s anonymous Enumeration reads
    // `childs.length` — see the note in `re8_make_loopback_interface`.
    let ni_cid = ctx
        .class_id_by_name("java/net/NetworkInterface")
        .unwrap_or(ClassId::new(0));
    let empty_childs = ctx.new_ref_array(ni_cid, 0);
    let iface = ctx.read_native_pin(iface_pin, iface0);
    ctx.set_field_by_name(iface, "childs", Value::Object(Some(empty_childs)));
    // The ctor also leaves `displayName` null (the real JDK's `getAll0` fills
    // it in). On Linux the display name IS the interface name.
    let display = ctx.create_string(host.display_name.as_deref().unwrap_or(&host.name));
    let iface = ctx.read_native_pin(iface_pin, iface0);
    ctx.set_field_by_name(iface, "displayName", Value::Object(Some(display)));
    let iface = ctx.read_native_pin(iface_pin, iface0);
    ctx.unpin_native_roots(base_pin);
    Ok(Some(iface))
}

/// `getAll()` — one REAL-layout carrier per host interface.
///
/// Falls back to the single loopback carrier when the host cannot be
/// enumerated. An EMPTY array is never returned while any interface exists:
/// `getNetworkInterfaces()` turns that into
/// `SocketException("No network interfaces configured")`, which Gradle's
/// `InetAddressFactory` escalates into "Could not determine a usable wildcard
/// IP for this machine", killing every user-home-scope service.
pub fn re8_all_interfaces(ctx: &mut dyn NativeContext) -> ObjectRef {
    let ni_cid = ctx
        .class_id_by_name("java/net/NetworkInterface")
        .unwrap_or(ClassId::new(0));
    let hosts = re8_scan_host_ifaces();
    // Pin each finished carrier: the next one's construction, and the array
    // allocation below, both allocate and can relocate it.
    let mut pinned: Vec<(usize, ObjectRef)> = Vec::new();
    let mut base_pin: Option<usize> = None;
    for host in &hosts {
        if let Ok(Some(iface)) = re8_make_interface(ctx, host) {
            let pin = ctx.pin_native_root(iface);
            if base_pin.is_none() {
                base_pin = Some(pin);
            }
            pinned.push((pin, iface));
        }
    }
    if pinned.is_empty() {
        if let Some(base) = base_pin {
            ctx.unpin_native_roots(base);
        }
        let Some(loopback) = re8_make_loopback_interface(ctx) else {
            return ctx.new_ref_array(ni_cid, 0);
        };
        let pin = ctx.pin_native_root(loopback);
        let arr = ctx.new_ref_array(ni_cid, 1);
        let iface = ctx.read_native_pin(pin, loopback);
        ctx.set_array_element(arr, 0, Value::Object(Some(iface)));
        ctx.unpin_native_roots(pin);
        return arr;
    }
    let arr = ctx.new_ref_array(ni_cid, pinned.len());
    for (i, (pin, iface)) in pinned.iter().enumerate() {
        let current = ctx.read_native_pin(*pin, *iface);
        ctx.set_array_element(arr, i, Value::Object(Some(current)));
    }
    if let Some(base) = base_pin {
        ctx.unpin_native_roots(base);
    }
    arr
}

/// `getByName0` / `getByIndex0` / `getByInetAddress0` share this: find the host
/// interface a predicate selects and build its carrier, falling back to the
/// loopback carrier when the host cannot be enumerated and the predicate is
/// asking about loopback.
pub fn re8_find_interface(
    ctx: &mut dyn NativeContext,
    select: impl Fn(&Re8HostIface) -> bool,
    loopback_fallback: bool,
) -> Result<Option<ObjectRef>, MethodCallFailed> {
    let hosts = re8_scan_host_ifaces();
    if hosts.is_empty() {
        return if loopback_fallback {
            Ok(re8_make_loopback_interface(ctx))
        } else {
            Ok(None)
        };
    }
    let Some(host) = hosts.into_iter().find(select) else {
        return Ok(None);
    };
    Ok(re8_make_interface(ctx, &host)?)}

/// Build the single REAL-layout loopback `NetworkInterface` ("lo", index 1,
/// the loopback address, no sub-interfaces).
///
/// Constructed through the package-private `NetworkInterface(String,int,
/// InetAddress[])` constructor so the real `getInetAddresses()`/`toString()`
/// bytecode reads the right fields — a synthetic 5-slot object of that class
/// breaks them (`arraylength null` in `NetworkInterface$1`; see the note at the
/// top of `register_re8_network_interface`). Returns `None` when any step
/// fails; the caller then answers "no interfaces" / "no such interface".
///
/// GC note: the returned reference is NOT pinned. A caller that allocates again
/// before using it must pin it across that allocation.
pub fn re8_make_loopback_interface(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let lo_addr = match ctx.invoke(
        "java/net/InetAddress",
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return None,
    };
    let lo_pin = ctx.pin_native_root(lo_addr);
    let addr_cid = ctx
        .class_id_by_name("java/net/InetAddress")
        .unwrap_or(ClassId::new(0));
    let addrs = ctx.new_ref_array(addr_cid, 1);
    let lo_addr = ctx.read_native_pin(lo_pin, lo_addr);
    ctx.set_array_element(addrs, 0, Value::Object(Some(lo_addr)));
    let addrs_pin = ctx.pin_native_root(addrs);
    let name = ctx.create_string(RE8_LOOPBACK_NAME);
    let name_pin = ctx.pin_native_root(name);
    let iface = match ctx.new_object("java/net/NetworkInterface") {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(lo_pin);
            ctx.unpin_native_roots(addrs_pin);
            ctx.unpin_native_roots(name_pin);
            return None;
        }
    };
    let iface_pin = ctx.pin_native_root(iface);
    let name = ctx.read_native_pin(name_pin, name);
    let addrs = ctx.read_native_pin(addrs_pin, addrs);
    let _ = ctx.invoke(
        "java/net/NetworkInterface",
        "<init>",
        "(Ljava/lang/String;I[Ljava/net/InetAddress;)V",
        &[
            Value::Object(Some(iface)),
            Value::Object(Some(name)),
            Value::Int(RE8_LOOPBACK_INDEX),
            Value::Object(Some(addrs)),
        ],
    );
    let iface = ctx.read_native_pin(iface_pin, iface);
    let ni_cid = ctx
        .class_id_by_name("java/net/NetworkInterface")
        .unwrap_or(ClassId::new(0));
    // The package-private `(String,int,InetAddress[])` ctor leaves the `childs`
    // field null — the real JDK's native `getAll0` is what populates it.
    // `NetworkInterface.getSubInterfaces()` returns an anonymous Enumeration
    // whose `hasMoreElements()` reads `childs.length`, so a null `childs` throws
    // `NullPointerException: arraylength null` (NetworkInterface$1). That kills
    // `NetworkUtils.<clinit>` (its `addAllInterfaces` recursion calls
    // `Collections.list(intf.getSubInterfaces())`) with an
    // ExceptionInInitializerError in every Elasticsearch ESTestCase that touches
    // networking. A loopback interface genuinely has no sub-interfaces, so an
    // empty `NetworkInterface[]` is the faithful value.
    let empty_childs = ctx.new_ref_array(ni_cid, 0);
    let iface = ctx.read_native_pin(iface_pin, iface);
    ctx.set_field_by_name(iface, "childs", Value::Object(Some(empty_childs)));
    let iface = ctx.read_native_pin(iface_pin, iface);
    ctx.unpin_native_roots(lo_pin);
    ctx.unpin_native_roots(addrs_pin);
    ctx.unpin_native_roots(name_pin);
    ctx.unpin_native_roots(iface_pin);
    Some(iface)
}

pub fn register_re8_network_interface(r: &mut NativeMethodRegistry) {
    let ni = "java/net/NetworkInterface";

    // NOTE: We do NOT register synthetic `getNetworkInterfaces` /
    // `networkInterfaces` natives. In real-JDK mode the Java methods call
    // native `getAll()` (registered below), and the real bytecode wraps the
    // result. Returning *synthetic* NetworkInterface objects from here would
    // break the real JDK's `getInetAddresses()`, whose `addrs` field is in a
    // different slot than our synthetic layout — `arraylength null` NPE inside
    // NetworkInterface$1. `getAll()` sidesteps that by building REAL-layout
    // carriers instead (`re8_make_interface` / `re8_make_loopback_interface`).
    //
    // STALE-PREMISE WARNING for anyone reading downstream comments: `getAll()`
    // does NOT return an empty array and does NOT hand out a single loopback
    // interface any more — it enumerates the host (`re8_scan_host_ifaces`).
    // `getNetworkInterfaces()` therefore does not throw SocketException("No
    // network interfaces configured"), and `findFirstNonLoopbackAddress`-style
    // callers do find a non-loopback address. Any shim that justifies itself
    // with either premise is working from an outdated one.

    // `NetworkInterface.<clinit>` calls the JNI library initializer
    // `init()V` — unregistered it surfaced as UnsatisfiedLinkError and
    // killed any class-init touching NetworkInterface (Gradle's user-home
    // services during ProjectBuilder bootstrap). The real init only caches
    // JNI field IDs; a no-op is faithful.
    r.register(
        ni,
        "init",
        "()V",
        |_ctx, _args| Ok(None),
        NativeKind::Bridge,
    );

    // IMPLEMENTED (wave 4). This used to return an unconditional `null`,
    // justified by "`getAll()` hands out exactly one interface, loopback, which
    // genuinely has no hardware address". That premise is gone: `getAll()` now
    // enumerates the host, so most receivers reaching this method DO have a
    // MAC, and a blanket null is what made CratonVM report no hardware address
    // where HotSpot reports a real one.
    //
    // `null` remains the answer when the interface really has none — the spec'd
    // value for "the address does not exist or is not accessible" — but it is
    // now derived rather than assumed. Note what is deliberately NOT returned:
    // the pre-wave-3 all-zero 00:00:00:00:00:00, which is a syntactically valid
    // address, so UUID-v1 generators and cluster-member-id derivations accept it
    // and every host ends up with the same identity (see `re8_parse_mac`).
    //
    // NOTE this is the copy that serves DEFAULT real-JDK mode. The twin in
    // `phases_late/nio_file.rs` (`register_p61_net`) is reachable only through
    // `register_synthetic_overrides`, i.e. `--synthetic-jdk`.
    r.register(ni, "getHardwareAddress", "()[B", |ctx, args| {
        let name = re8_receiver_name(ctx, args);
        let mac = re8_host_iface_by_name(&name)
            .map(|host| host.mac)
            .unwrap_or_default();
        re8_mac_array(ctx, &mac)
    }, NativeKind::SyntheticStub);
    // Real per-interface MTU. Linux publishes it as a plain file — the same
    // number SIOCGIFMTU returns — so read it rather than guessing; no FFI
    // needed. The fallback is the per-platform LOOPBACK MTU (65536 on Linux,
    // 16384 on macOS, 1500 on Windows), because loopback is the only interface
    // this module can still mint when the host cannot be enumerated. A flat
    // 1500 was wrong on two of those three.
    r.register(ni, "getMTU", "()I", |ctx, args| {
        let name = re8_receiver_name(ctx, args);
        let mtu = re8_host_iface_by_name(&name)
            .and_then(|host| host.mtu)
            .unwrap_or_else(re8_loopback_mtu);
        Ok(Some(Value::Int(mtu)))
    }, NativeKind::SyntheticStub);

    // Low-level "0" suffixed natives (JDK internals). All static, so index 0 is
    // the interface NAME, not a receiver. Each now answers from the host's real
    // `IFF_*` flag word instead of restating loopback's answers; the fallback
    // (host not enumerable) is still the loopback answer, which is what the
    // fallback carrier `getAll()` mints in that case genuinely reports.
    //
    // These mirror the real JNI bodies: `isUp0` is `IFF_UP && IFF_RUNNING`,
    // `isLoopback0` is `IFF_LOOPBACK`, `isP2P0` is `IFF_POINTOPOINT`,
    // `supportsMulticast0` is `IFF_MULTICAST`.
    r.register(
        ni,
        "isUp0",
        "(Ljava/lang/String;I)Z",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 0);
            let up = match re8_host_iface_by_name(&name) {
                Some(host) => host.flags & RE8_IFF_UP != 0 && host.flags & RE8_IFF_RUNNING != 0,
                // Loopback is always up.
                None => true,
            };
            Ok(Some(Value::Int(i32::from(up))))
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "isLoopback0",
        "(Ljava/lang/String;I)Z",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 0);
            let index = args.get(1).and_then(Value::as_int).unwrap_or(0);
            let loopback = match re8_host_iface_by_name(&name) {
                Some(host) => host.flags & RE8_IFF_LOOPBACK != 0,
                None => name == RE8_LOOPBACK_NAME || index == RE8_LOOPBACK_INDEX,
            };
            Ok(Some(Value::Int(i32::from(loopback))))
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "isP2P0",
        "(Ljava/lang/String;I)Z",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 0);
            let p2p = re8_host_iface_by_name(&name)
                .map(|host| host.flags & RE8_IFF_POINTOPOINT != 0)
                // Loopback is not point-to-point.
                .unwrap_or(false);
            Ok(Some(Value::Int(i32::from(p2p))))
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "supportsMulticast0",
        "(Ljava/lang/String;I)Z",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 0);
            let multicast = re8_host_iface_by_name(&name)
                .map(|host| host.flags & RE8_IFF_MULTICAST != 0)
                // Loopback does not carry IFF_MULTICAST.
                .unwrap_or(false);
            Ok(Some(Value::Int(i32::from(multicast))))
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "getMTU0",
        "(Ljava/lang/String;I)I",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 0);
            let mtu = re8_host_iface_by_name(&name)
                .and_then(|host| host.mtu)
                .unwrap_or_else(re8_loopback_mtu);
            Ok(Some(Value::Int(mtu)))
        },
        NativeKind::Bridge,
    );
    // `getMacAddr0(byte[] inAddr, String name, int ind)` — static, so the name
    // is argument 1. `inAddr` only disambiguates which binding the caller meant
    // on platforms whose lookup is per-address; the Linux/`/sys` answer is
    // per-interface, so it is not needed.
    r.register(
        ni,
        "getMacAddr0",
        "([BLjava/lang/String;I)[B",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 1);
            let mac = re8_host_iface_by_name(&name)
                .map(|host| host.mac)
                .unwrap_or_default();
            re8_mac_array(ctx, &mac)
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "getAll",
        "()[Ljava/net/NetworkInterface;",
        |ctx, _args| {
            let arr = re8_all_interfaces(ctx);
            Ok(Some(Value::Object(Some(arr))))
        },
        NativeKind::Bridge,
    );
    // These three must agree with `getAll()` — a blanket null here used to
    // contradict it outright (`getByName("lo")` reported "no such interface"
    // for the interface the very same class had just enumerated). They now
    // search the same host enumeration `getAll()` uses, and fall back to the
    // loopback carrier only where `getAll()` itself does. Null stays the answer
    // for an interface that does not exist — the spec'd "no such interface".
    r.register(
        ni,
        "getByName0",
        "(Ljava/lang/String;)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let name = re8_name_arg(ctx, args, 0);
            let wanted = name.clone();
            let iface = re8_find_interface(
                ctx,
                move |host| host.name == wanted,
                name == RE8_LOOPBACK_NAME,
            )?;
            Ok(Some(Value::Object(iface)))
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "getByInetAddress0",
        "(Ljava/net/InetAddress;)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let Some(Value::Object(Some(addr))) = args.first().copied() else {
                return Ok(Some(Value::Object(None)));
            };
            let ip_str = inet_addr_field_string_or(ctx, addr, IA_ADDR, "");
            let Ok(ip) = ip_str.trim_matches(&['[', ']'][..]).parse::<IpAddr>() else {
                return Ok(Some(Value::Object(None)));
            };
            let iface =
                re8_find_interface(ctx, move |host| host.addrs.contains(&ip), ip.is_loopback())?;
            Ok(Some(Value::Object(iface)))
        },
        NativeKind::Bridge,
    );
    // `boundInetAddress0(InetAddress)` — "is this address configured on some
    // local interface?". This one answers a QUESTION rather than handing back a
    // `NetworkInterface`, so it can consult the host directly instead of being
    // restricted to the single carrier its `getByXxx0` siblings can build.
    // A constant `false` is the dangerous direction: real
    // `InetAddress.isAnyLocalAddress`/bind-validation callers read it as "that
    // address is not mine" and reject a bind or a same-host shortcut that
    // would have worked — for 127.0.0.1, always. Answer from the same local-IP
    // enumeration `getAll()` uses (`re8_enumerate_local_ips` now folds in the
    // host's configured addresses, so a secondary NIC is no longer a miss).
    r.register(
        ni,
        "boundInetAddress0",
        "(Ljava/net/InetAddress;)Z",
        |ctx, args| {
            let Some(Value::Object(Some(addr))) = args.first().copied() else {
                return Ok(Some(Value::Int(0)));
            };
            let ip_str = inet_addr_field_string_or(ctx, addr, IA_ADDR, "");
            let Ok(ip) = ip_str.trim_matches(&['[', ']'][..]).parse::<IpAddr>() else {
                return Ok(Some(Value::Int(0)));
            };
            // Loopback and the wildcard are bound on every host by definition;
            // the enumeration below can miss the wildcard entirely.
            let bound = ip.is_loopback()
                || match ip {
                    IpAddr::V4(v) => v.is_unspecified(),
                    IpAddr::V6(v) => v.is_unspecified(),
                }
                || re8_enumerate_local_ips().contains(&ip);
            Ok(Some(Value::Int(i32::from(bound))))
        },
        NativeKind::Bridge,
    );
    r.register(
        ni,
        "getByIndex0",
        "(I)Ljava/net/NetworkInterface;",
        |ctx, args| {
            let index = args.first().and_then(Value::as_int).unwrap_or(0);
            let iface = re8_find_interface(
                ctx,
                move |host| host.index == index,
                index == RE8_LOOPBACK_INDEX,
            )?;
            Ok(Some(Value::Object(iface)))
        },
        NativeKind::Bridge,
    );

    // REMOVED (wave 4): the R76 `HostInfoEnvironmentPostProcessor.
    // postProcessEnvironment` no-op.
    //
    // Its premise was that `getAll()` returned `[]`, so
    // `InetUtils.findFirstNonLoopbackHostInfo` ->
    // `NetworkInterface.getNetworkInterfaces()` threw SocketException("No
    // network interfaces configured"); Spring caught it and the catch path
    // walked through enough recursive `Binder.bind` work to hit a SEGV in
    // JIT-compiled `ConfigurationPropertyName` parsing.
    //
    // Both halves of that premise are now dead. `getAll()` enumerates the
    // host's real interfaces, so `getNetworkInterfaces()` does not throw AND
    // `findFirstNonLoopbackAddress` now actually finds a non-loopback address
    // instead of exhausting the enumeration — the catch path is never entered.
    // The shim's cost was real and unconditional: it dropped
    // `spring.cloud.client.ip-address` and `spring.cloud.client.hostname`,
    // which the real post-processor publishes and Eureka/Ribbon registration
    // reads. Letting the real bytecode run restores them.
    //
    // If a Spring Cloud boot regresses with a SEGV in
    // `ConfigurationPropertyName`, that is the JIT bug this shim was hiding —
    // fix it there, do not reinstate the bypass.
}

// ===========================================================================
// RE.9 — java.nio.channels.Selector
// ===========================================================================

pub fn register_re9_nio_selector(r: &mut NativeMethodRegistry) {
    let sel = "java/nio/channels/Selector";

    r.register(sel, "select", "(J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => return Err(iae("select(J) missing argument")),
        };
        if raw < 0 {
            return Err(iae(format!("negative timeout {raw}")));
        }
        if raw == 0 {
            match ctx.invoke_virtual(this, "selectNow", "()I", &[]) {
                Ok(Some(v)) => Ok(Some(v)),
                _ => Ok(Some(Value::Int(0))),
            }
        } else {
            let deadline = std::time::Instant::now() + Duration::from_millis(raw as u64);
            let mut total = 0i32;
            // GC-safety: `selectNow` is real bytecode, dispatched on `this`
            // every turn of a loop that spins until a deadline.
            let this_pin = ctx.pin_native_root(this);
            while std::time::Instant::now() < deadline {
                let this = ctx.read_native_pin(this_pin, this);
                if let Ok(Some(Value::Int(n))) = ctx.invoke_virtual(this, "selectNow", "()I", &[]) {
                    if n > 0 {
                        total = n;
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(1));
                // gc-common w20-d: read back after `selectNow` (Java); the
                // open check read the selector through its pre-call address.
                let this = ctx.read_native_pin(this_pin, this);
                if ctx.get_field(this, SEL_OPEN).as_int().unwrap_or(1) == 0 {
                    break;
                }
            }
            Ok(Some(Value::Int(total)))
        }
    }, NativeKind::SyntheticStub);

    r.register(sel, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SEL_OPEN)))
    }, NativeKind::SyntheticStub);
}

// ===========================================================================
// RE.10 — com.sun.net.httpserver.HttpServer
// ===========================================================================

pub struct HttpHandlerEntry {
    pub path_prefix: String,
    pub handler: ObjectRef,
    /// The `HttpContext` object `createContext` handed back to the caller.
    ///
    /// Kept so the request dispatcher can consult the context's
    /// `Authenticator` before it runs the handler:
    /// `HttpContext.setAuthenticator` stores the authenticator in a side
    /// registry keyed by the *context object's* identity (see
    /// `phases_late::net_channels`), so the context instance — not just its
    /// path — has to survive here. Rooted and relocated alongside `handler`
    /// by the two GC helpers below; without that a moving young GC would leave
    /// this pointing at a vacated slot and the authenticator lookup would read
    /// a foreign identity hash.
    pub context: ObjectRef,
}

pub struct ServerState {
    // Behind a Mutex so stop() can `take()` (and thus close) the OS listener
    // SYNCHRONOUSLY — a stopped host must immediately refuse connections so a
    // round-robin client retries another node instead of connecting into a dead
    // server (ES MultipleHosts stopRandomHost). The accept loop locks it briefly
    // per (non-blocking) accept; taking it makes the next iteration exit.
    pub listener: Mutex<Option<TcpListener>>,
    pub running: AtomicBool,
    pub handlers: Mutex<Vec<HttpHandlerEntry>>,
    pub bound_port: AtomicI32,
    /// Handles on the kept-alive connections, so `stop()` can shut them. Each
    /// connection thread parks in a read between requests; without this a
    /// stopped server would leave them open until the idle interval, and a
    /// pooling client would hand the next server's first request to a dead one.
    /// `Scratch`: every acquisition is a single insert/remove/drain, released
    /// before the request is ever handed to a Java `HttpHandler` — never held
    /// across anything.
    pub conns: OrderedPlMutex<HashMap<u64, TcpStream>>,
    pub next_conn: AtomicU64,
    /// The `vm_identity` of the VM that created this server. `server_registry`
    /// is process-wide and server ids are process-unique; only this VM's
    /// collections root and remap `handlers` (gc-common w8-a, row
    /// `http-handlers`).
    pub vm: usize,
}

pub fn server_registry() -> &'static Mutex<HashMap<i32, std::sync::Arc<ServerState>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, std::sync::Arc<ServerState>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn next_server_id() -> i32 {
    static COUNTER: AtomicI32 = AtomicI32::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// VM teardown for the `com.sun.net.httpserver` servers VM `vm` created and
/// never stopped (gc-common w16-a, `common-w15a-http-server-outlives-its-vm`):
/// `HttpServer.stop` minus the Java-side `re10_dispatch_pending`, which cannot
/// run without the VM. Called from [`forget_vm_socket_side_tables`], i.e. from
/// `lib.rs::forget_vm_native_root_stores`. Returns how many servers went.
/// Idempotent; other VMs' servers are left alone.
///
/// `server_registry` is process-wide, and `HttpServer.stop` was the only thing
/// that removed a row, so a VM destroyed with a started server left behind its
/// bound port (a second VM in the process could not bind it), an OS accept
/// thread parsing requests into a queue no dispatcher would ever drain (the
/// clients hung until their own timeouts), and handler / context `ObjectRef`s
/// into the freed heap. `forget_vm_native_root_stores` used to exclude this
/// registry on purpose, because dropping a row closes a LIVE listener; at
/// teardown the listener is live but no longer served: the dispatchers are VM
/// threads, and teardown runs after the VM's threads have stopped.
///
/// Per server: `running` goes false (the accept thread's loop condition), the
/// listener is taken and dropped (the port is released and the accept wait
/// ends), the queued requests are dropped (their reply senders release the
/// kept-alive connection threads), every kept-alive connection is shut down,
/// and the handler rows are cleared. The registry lock and a server's own
/// locks are never held together, as in `stop()`.
pub fn forget_vm_http_servers(vm: usize) -> usize {
    let mine: Vec<(i32, std::sync::Arc<ServerState>)> = {
        let mut reg = server_registry().lock();
        let ids: Vec<i32> = reg
            .iter()
            .filter(|(_, state)| state.vm == vm)
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| reg.remove(&id).map(|state| (id, state)))
            .collect()
    };
    if mine.is_empty() {
        return 0;
    }
    for (_, state) in &mine {
        state.running.store(false, Ordering::SeqCst);
        state.listener.lock().take();
    }
    let discarded: Vec<Vec<PendingRequest>> = {
        let mut queue = request_queue().lock();
        mine.iter().filter_map(|(id, _)| queue.remove(id)).collect()
    };
    // Outside the queue lock: dropping a request closes its one-shot stream.
    drop(discarded);
    request_queue_ready().notify_all();
    for (_, state) in &mine {
        let conns: Vec<TcpStream> = state.conns.lock().drain().map(|(_, c)| c).collect();
        for conn in conns {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        state.handlers.lock().clear();
    }
    mine.len()
}

/// GC root scan for the synthetic `com.sun.net.httpserver` server registry.
///
/// Each registered `HttpHandlerEntry.handler` is a live Java `HttpHandler`
/// ObjectRef stored OUTSIDE the Java heap (in this native `server_registry`),
/// so the collector has no other view of it. Without rooting it here a moving
/// young GC could either reclaim the handler (the only strong reference is this
/// native map) or relocate it and leave the stored ObjectRef dangling — and the
/// per-request dispatcher (`re10_dispatch_pending`) then `invoke_virtual`s a
/// stale receiver, which resolves to `java/lang/Object` and fails with
/// `NoSuchMethodError: java/lang/Object.handle(...)`. Observed as an intermittent
/// storm under `-Xmx1g` GC pressure (ES `RestClientSingleHostIntegTests`
/// `testManyAsyncRequests`), GC-frequency-dependent (green at large heaps). The
/// companion `gc_update_re10_handler_refs` re-points the stored refs after a
/// move; this scan keeps them live across the collection.
///
/// Mirrors the established native-root pattern (locale / logmanager / jboss_msc).
/// Arcs are cloned out from under the registry lock first so the registry lock
/// and the per-server `handlers` lock are never held simultaneously.
///
/// Only servers `vm` created (`ServerState::vm`): the registry is shared by
/// every VM in the process (gc-common w8-a).
pub fn gc_scan_re10_handler_roots(vm: usize, out: &mut Vec<ObjectRef>) {
    let states: Vec<std::sync::Arc<ServerState>> = {
        let reg = server_registry().lock();
        reg.values().filter(|s| s.vm == vm).cloned().collect()
    };
    for state in states {
        let hs = state.handlers.lock();
        for e in hs.iter() {
            out.push(e.handler);
            // Same reasoning for the context: once the application drops the
            // reference `createContext` returned, this native map is the only
            // thing that keeps it alive, and the dispatcher dereferences it
            // once per request to read its authenticator.
            out.push(e.context);
        }
    }
}

/// Companion to [`gc_scan_re10_handler_roots`]: after a moving collection,
/// re-point every stored `HttpHandlerEntry.handler` to its relocated address so
/// the dispatcher invokes the live handler, not a vacated from-space slot. A
/// no-op when nothing moved (empty `pointer_map`) or for handlers the collector
/// left in place (absent from the map).
pub fn gc_update_re10_handler_refs(vm: usize, pointer_map: &cratonvm_types::PointerMap) {
    if pointer_map.is_empty() {
        return;
    }
    let states: Vec<std::sync::Arc<ServerState>> = {
        let reg = server_registry().lock();
        reg.values().filter(|s| s.vm == vm).cloned().collect()
    };
    for state in states {
        let mut hs = state.handlers.lock();
        for e in hs.iter_mut() {
            let old = e.handler.as_ptr() as usize;
            if let Some(&new) = pointer_map.get(&old) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: `new` is a relocated address produced by the collector's
                // pointer map for this exact object; it points at a valid header.
                e.handler = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
            let old_ctx = e.context.as_ptr() as usize;
            if let Some(&new) = pointer_map.get(&old_ctx) {
                debug_assert!(new != 0, "GC pointer map contains null address");
                // SAFETY: as above — a relocated address for this exact object.
                e.context = unsafe { ObjectRef::from_raw(new as *mut u8) };
            }
        }
    }
}

pub struct PendingRequest {
    /// The connection itself, on the one-shot path only: the dispatcher writes
    /// the response to it and closes it (`re10_send_response`). `None` on a
    /// kept-alive connection, whose own thread owns the socket and receives the
    /// response through `reply`.
    pub stream: Option<TcpStream>,
    /// Endpoint addresses, captured when the connection was accepted so the
    /// exchange can report them without the socket.
    pub local: Option<SocketAddr>,
    pub peer: Option<SocketAddr>,
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Where the serialised response goes on a kept-alive connection. Dropping
    /// it unanswered (a stopped server discarding its queue) wakes the
    /// connection thread with an error, which closes the connection.
    pub reply: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// The request allows the connection to persist: HTTP/1.1 without
    /// `Connection: close`.
    pub persistent: bool,
    /// The request line said HTTP/1.1. Decides whether a closing response
    /// announces `Connection: close` (see the dispatcher).
    pub http11: bool,
}

pub fn request_queue() -> &'static Mutex<HashMap<i32, Vec<PendingRequest>>> {
    static INSTANCE: OnceLock<Mutex<HashMap<i32, Vec<PendingRequest>>>> = OnceLock::new();
    INSTANCE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Signalled on every enqueue, so an idle dispatcher wakes for a request
/// instead of finishing a fixed sleep first. See `re10_serve_loop_run`.
pub fn request_queue_ready() -> &'static parking_lot::Condvar {
    static CV: OnceLock<parking_lot::Condvar> = OnceLock::new();
    CV.get_or_init(parking_lot::Condvar::new)
}

/// Queue a parsed request for the dispatchers and wake one.
///
/// `drop_if_stopped` is set on the keep-alive path: the check is made UNDER the
/// queue lock, and `stop()` clears `running` before it removes the server's
/// queue under the same lock, so a request can never be parked behind a
/// dispatcher that has already exited. Dropping it drops its `reply` sender,
/// which is what releases the connection thread waiting on it.
pub fn re10_enqueue(
    server_id: i32,
    state: &ServerState,
    pending: PendingRequest,
    drop_if_stopped: bool,
) {
    let mut q = request_queue().lock();
    if drop_if_stopped && !state.running.load(Ordering::SeqCst) {
        return;
    }
    q.entry(server_id).or_default().push(pending);
    drop(q);
    request_queue_ready().notify_one();
}

/// `CRATONVM_HTTPSRV_KEEPALIVE` (default ON). `=0` restores the one-shot
/// server: `Connection: close` on every response, one parse thread per
/// connection and one responder thread per response.
pub fn httpsrv_keepalive_enabled() -> bool {
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        cratonvm_types::flags::runtime_var("CRATONVM_HTTPSRV_KEEPALIVE")
            .map(|v| {
                let v = v.trim();
                !(v == "0" || v.eq_ignore_ascii_case("false") || v.eq_ignore_ascii_case("off"))
            })
            .unwrap_or(true)
    })
}

/// How long a kept-alive connection may sit idle between requests: the JDK's
/// own `sun.net.httpserver.idleInterval` default.
pub const HTTPSRV_KEEPALIVE_IDLE: Duration = Duration::from_secs(30);

/// `Connection: close` anywhere in the request's `Connection` header(s).
pub fn request_asks_close(headers: &[(String, String)]) -> bool {
    headers.iter().any(|(k, v)| {
        k.eq_ignore_ascii_case("connection")
            && v.split(',').any(|t| t.trim().eq_ignore_ascii_case("close"))
    })
}

/// Total length of a chunked body's framing — through the terminating zero
/// chunk, any trailer fields, and the final empty line — or `None` while any
/// of it has yet to arrive.
///
/// `http_decode_chunked` stops at the zero-size chunk and does not report how
/// much it consumed, which is right for a body that ends the connection and
/// wrong for one followed by another request: the final CRLF and any trailers
/// would be read as the start of the next request line.
pub fn http_chunked_request_len(data: &[u8]) -> Option<usize> {
    const MAX_CHUNK: usize = 64 * 1024 * 1024;
    let mut pos = 0usize;
    loop {
        let nl = data[pos..].windows(2).position(|w| w == b"\r\n")?;
        let line = std::str::from_utf8(&data[pos..pos + nl]).ok()?;
        let size = line.split(';').next().unwrap_or("").trim();
        if size.is_empty() || !size.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let n = usize::from_str_radix(size, 16)
            .ok()
            .filter(|&n| n <= MAX_CHUNK)?;
        pos += nl + 2;
        if n == 0 {
            loop {
                let nl = data[pos..].windows(2).position(|w| w == b"\r\n")?;
                pos += nl + 2;
                if nl == 0 {
                    return Some(pos);
                }
            }
        }
        if data.len() < pos + n + 2 {
            return None;
        }
        pos += n + 2;
    }
}

/// VULN-FIX [nb-net-phase-e]: configurable upper bound on the size of an inbound
/// HTTP request body for the embedded com.sun.net.httpserver. Previously
/// `parse_http_request` read up to the client-supplied `Content-Length` with NO
/// upper bound, allowing a remote peer to exhaust process memory (DoS) by
/// advertising (and streaming) an enormous body. We now cap the accepted body
/// and reject anything larger with a 413 response.
///
/// The cap is read once from `CRATONVM_HTTP_MAX_BODY` (bytes); it defaults to
/// 8 MiB, which is comfortably larger than any normal request the test suite
/// issues while still bounding worst-case allocation. A value of 0 or an
/// unparseable value falls back to the default.
pub fn http_max_request_body() -> usize {
    const DEFAULT: usize = 8 * 1024 * 1024; // 8 MiB
    crate::nbflags().http_max_body.unwrap_or(DEFAULT)
}

/// VULN-FIX [nb-net-phase-e]: best-effort write of a fixed minimal HTTP response
/// to a peer we are about to reject (e.g. 413 for an oversized body, 400 for a
/// malformed/duplicate Content-Length). Used so that the parse path can refuse a
/// request cheaply without allocating its body and still tell the client why.
pub fn http_reject_and_close(stream: &mut TcpStream, status: i32) {
    let body = http_reason(status);
    let mut resp = Vec::with_capacity(96 + body.len());
    let _ = write!(
        &mut resp,
        "HTTP/1.1 {status} {body}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(&resp);
    let _ = stream.flush();
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// Parse one request from a connection that will be closed after it: the
/// one-shot server's parser, unchanged in behaviour.
pub fn parse_http_request(mut stream: TcpStream) -> Option<PendingRequest> {
    let local = stream.local_addr().ok();
    let peer = stream.peer_addr().ok();
    let parsed = parse_http_request_core(&mut stream, Vec::new(), false, false)?;
    Some(PendingRequest {
        stream: Some(stream),
        local,
        peer,
        method: parsed.method,
        uri: parsed.uri,
        headers: parsed.headers,
        body: parsed.body,
        reply: None,
        persistent: false,
        http11: false,
    })
}

/// One request, parsed; `leftover` is whatever arrived after it on the wire.
pub struct ParsedRequest {
    pub method: String,
    pub uri: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub persistent: bool,
    pub http11: bool,
    pub leftover: Vec<u8>,
}

/// The request parser, over a borrowed connection.
///
/// * `carry` — bytes already read past the previous request (a pipelined
///   request may be entirely in here);
/// * `between_requests` — this is not the connection's first request, so the
///   peer may legitimately send nothing: wait `HTTPSRV_KEEPALIVE_IDLE` for the
///   next request to START, and close quietly if it never does;
/// * `persistent_conn` — another request may follow on this connection, so the
///   body must be framed exactly and the bytes after it returned rather than
///   discarded. The one-shot path passes `false` and keeps every historical
///   rule, including the ones that are only safe because the connection ends.
pub fn parse_http_request_core(
    stream: &mut TcpStream,
    carry: Vec<u8>,
    between_requests: bool,
    persistent_conn: bool,
) -> Option<ParsedRequest> {
    // The accept loop sets the LISTENER non-blocking; on Windows the accepted
    // stream inherits that mode, so a bare `read` returns WouldBlock the instant
    // the peer hasn't sent yet — which `Err(_) => return None` below would treat
    // as a dead connection and drop the socket. A well-behaved client that
    // connects slightly before it writes its request (e.g. the Apache NIO
    // reactor, which establishes the connection then writes on the next event
    // loop turn) would then see an immediate EOF / "Connection is closed". Force
    // the accepted stream BLOCKING so the read timeout below actually governs and
    // we wait for the request.
    stream.set_nonblocking(false).ok();
    let idle_wait = between_requests && carry.is_empty();
    stream
        .set_read_timeout(Some(if idle_wait {
            HTTPSRV_KEEPALIVE_IDLE
        } else {
            Duration::from_secs(10)
        }))
        .ok();
    let mut idle_armed = idle_wait;
    let mut buf = carry;
    buf.reserve(4096);
    let mut tmp = [0u8; 1024];
    // PERF [nb-net-phase-e]: scan only the newly-appended bytes for the
    // "\r\n\r\n" header terminator instead of re-running `buf.windows(4)` over
    // the whole accumulated buffer on every ~1 KiB read. Re-scanning from 0 each
    // read is O(n^2) in header size. `scanned` records how many leading bytes
    // have already been checked; each read we restart the window scan 3 bytes
    // before that point so a terminator straddling the previous/new boundary is
    // still detected, then capture its absolute position so the post-loop step
    // need not re-scan either. The detection result is identical to the original
    // full-buffer `windows(4).position(...)`.
    let mut scanned = 0usize;
    // A pipelined request can already be complete in the carried bytes.
    let mut sep: Option<usize> = buf.windows(4).position(|w| w == b"\r\n\r\n");
    if sep.is_none() {
        scanned = buf.len().saturating_sub(3);
    }
    if !buf.is_empty() && !buf[0].is_ascii_uppercase() {
        return None;
    }
    while sep.is_none() {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => {
                if idle_armed {
                    // The next request has started; the rest of it gets the
                    // ordinary bound, not the idle interval.
                    idle_armed = false;
                    stream.set_read_timeout(Some(Duration::from_secs(10))).ok();
                }
                buf.extend_from_slice(&tmp[..n]);
                // Fast-reject non-HTTP traffic: every HTTP request line starts
                // with an uppercase-ASCII method token (GET/POST/PUT/...). A TLS
                // ClientHello (0x16 ...) or other binary garbage never will, so
                // bail immediately instead of blocking the (now-blocking) read
                // until the timeout. Without this, an HTTPS client against the
                // plaintext synthetic server (com.sun.net.httpserver.HttpsServer
                // is not TLS-capable here) would hang the whole suite waiting for
                // a ServerHello that never comes — RestClientBuilderIntegTests.
                if !buf.is_empty() && !buf[0].is_ascii_uppercase() {
                    return None;
                }
                // Restart 3 bytes before the previously-scanned end so a
                // terminator split across the boundary is not missed (saturating
                // so the first read starts at 0).
                let start = scanned.saturating_sub(3);
                if let Some(rel) = buf[start..].windows(4).position(|w| w == b"\r\n\r\n") {
                    sep = Some(start + rel);
                    break;
                }
                // Everything except the trailing 3 bytes (which may begin a
                // boundary-straddling terminator) is now fully checked.
                scanned = buf.len().saturating_sub(3);
                if buf.len() > 1 << 20 {
                    return None;
                }
            }
            Err(_) => return None,
        }
    }
    let sep = sep?;
    let head = std::str::from_utf8(&buf[..sep]).ok()?;
    let mut lines = head.split("\r\n");
    let req_line = lines.next()?;
    let mut rl = req_line.splitn(3, ' ');
    let method = rl.next()?.to_string();
    let uri = rl.next()?.to_string();
    let http11 = rl.next()?.trim().eq_ignore_ascii_case("HTTP/1.1");
    let mut headers = Vec::new();
    // VULN-FIX [nb-net-phase-e]: track Content-Length as an Option and DETECT
    // duplicate/conflicting declarations instead of silently letting the last
    // one win. Two differing Content-Length values (or a malformed one) are a
    // classic request-smuggling vector, so we reject the request with 400.
    let mut content_length: Option<usize> = None;
    // VULN-FIX [nb-net-phase-e]: collect the full Transfer-Encoding declaration.
    // Previously this parser framed the body STRICTLY by Content-Length and never
    // inspected Transfer-Encoding, so a peer sending `Transfer-Encoding: chunked`
    // (with no Content-Length, or with a lying one) got content_length=0: we read
    // ZERO body bytes and left the entire chunked payload unread in the socket
    // buffer. On a keep-alive connection the next request then mis-parses that
    // leftover payload — classic request-smuggling / body desync (RFC 7230
    // §3.3.3). We now accumulate every transfer coding (a single value may be a
    // comma list, and multiple header lines are equivalent to one comma-joined
    // list per RFC 7230 §3.2.2) and resolve the framing after the loop.
    let mut transfer_codings: Vec<String> = Vec::new();
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("transfer-encoding") {
                for coding in v.split(',') {
                    let c = coding.trim();
                    if !c.is_empty() {
                        transfer_codings.push(c.to_ascii_lowercase());
                    }
                }
            }
            if k.eq_ignore_ascii_case("content-length") {
                // A header value may itself be a comma-separated list of equal
                // values (RFC 9110 §8.6); any unparseable or conflicting value
                // is treated as malformed.
                let parsed = v
                    .split(',')
                    .map(|p| p.trim())
                    .try_fold(None::<usize>, |acc, p| {
                        let n: usize = p.parse().ok()?;
                        match acc {
                            Some(prev) if prev != n => None, // conflicting list members
                            _ => Some(Some(n)),
                        }
                    })
                    .flatten();
                match parsed {
                    Some(n) => match content_length {
                        Some(prev) if prev != n => {
                            // Conflicting duplicate Content-Length headers.
                            http_reject_and_close(stream, 400);
                            return None;
                        }
                        _ => content_length = Some(n),
                    },
                    None => {
                        // Unparseable / list with differing values.
                        http_reject_and_close(stream, 400);
                        return None;
                    }
                }
            }
            headers.push((k, v));
        }
    }
    let max_body = http_max_request_body();
    // VULN-FIX [nb-net-phase-e]: resolve message framing per RFC 7230 §3.3.3.
    // Transfer-Encoding, when present, takes precedence over Content-Length and
    // determines how the body is delimited. Handle it BEFORE the Content-Length
    // path below.
    if !transfer_codings.is_empty() {
        // RFC 7230 §3.3.1: a sender MUST NOT apply chunked more than once, and
        // for a request to be framed by chunked it must be the FINAL coding.
        // We only support `chunked` (optionally as the sole/last coding) and the
        // no-op `identity`; anything else (gzip/deflate/compress, or chunked not
        // last) is something we cannot safely de-frame, so we reject rather than
        // guess at the body boundary (a guess is exactly the smuggling hazard).
        let last_is_chunked = transfer_codings.last().map(|c| c == "chunked") == Some(true);
        let chunked_count = transfer_codings.iter().filter(|c| *c == "chunked").count();
        let only_identity_or_chunked = transfer_codings
            .iter()
            .all(|c| c == "chunked" || c == "identity");
        if !last_is_chunked || chunked_count != 1 || !only_identity_or_chunked {
            // Unknown/unsupported transfer coding, or chunked applied more than
            // once / not last — reject instead of mis-framing the body.
            http_reject_and_close(stream, 400);
            return None;
        }
        // RFC 7230 §3.3.3 (3): if a message is received with BOTH a
        // Transfer-Encoding and a Content-Length, the Content-Length MUST be
        // treated as suspect — a strong signal of request smuggling. Reject
        // outright rather than trusting either framing.
        if content_length.is_some() {
            http_reject_and_close(stream, 400);
            return None;
        }
        // The chunked body may not have fully arrived with the header (we read
        // in ~1 KiB blocks above). Keep reading until the terminating zero-size
        // chunk `0\r\n\r\n` is present, bounding the accumulated raw size at the
        // cap so a peer can't stream unbounded data (chunked has no advertised
        // length) and exhaust memory. The cap also covers the inter-chunk
        // framing overhead, which is acceptable for a defensive upper bound.
        let mut raw = buf[sep + 4..].to_vec();
        if raw.len() > max_body {
            http_reject_and_close(stream, 413);
            return None;
        }
        // Completeness is decided by actually walking the chunk framing with the
        // shared decoder rather than by scanning for a `0\r\n\r\n` byte pattern:
        // that pattern can legitimately occur INSIDE chunk data, which would
        // truncate the body early. `http_decode_chunked` returns `Ok` only once
        // the full framing (through the terminating zero chunk) is present, so we
        // read more whenever it still errors — until we succeed, the peer closes,
        // the read times out, or we hit the byte cap.
        let mut rest: Vec<u8> = Vec::new();
        let body = loop {
            // On a connection that may carry another request the framing must
            // be complete through its final CRLF, and whatever follows is the
            // next request. The one-shot path keeps its historical rule.
            let decoded = if persistent_conn {
                match http_chunked_request_len(&raw) {
                    Some(n) => {
                        rest = raw[n..].to_vec();
                        http_decode_chunked(&raw[..n])
                    }
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "chunked body incomplete",
                    )),
                }
            } else {
                http_decode_chunked(&raw)
            };
            match decoded {
                Ok(b) => break b,
                Err(_) => match stream.read(&mut tmp) {
                    Ok(0) => {
                        // Peer closed before a complete, valid chunked body.
                        http_reject_and_close(stream, 400);
                        return None;
                    }
                    Ok(n) => {
                        raw.extend_from_slice(&tmp[..n]);
                        if raw.len() > max_body {
                            http_reject_and_close(stream, 413);
                            return None;
                        }
                    }
                    Err(_) => {
                        // Read timeout / I/O error with an incomplete body.
                        http_reject_and_close(stream, 400);
                        return None;
                    }
                },
            }
        };
        // The decoded payload must itself stay within the cap (the raw cap bounds
        // framing + data, but enforce on the decoded size too, defensively).
        if body.len() > max_body {
            http_reject_and_close(stream, 413);
            return None;
        }
        let persistent = http11 && !request_asks_close(&headers);
        return Some(ParsedRequest {
            method,
            uri,
            headers,
            body,
            persistent,
            http11,
            leftover: rest,
        });
    }
    let content_length = content_length.unwrap_or(0);
    // VULN-FIX [nb-net-phase-e]: bound the advertised body length BEFORE we read
    // or allocate anything. Without this a remote peer could send a huge
    // Content-Length and stream gigabytes, exhausting process memory.
    if content_length > max_body {
        http_reject_and_close(stream, 413);
        return None;
    }
    // Any bytes already pulled in while reading the header also count toward the
    // bounded body. Clamp the initial slice so a peer can't smuggle past the cap
    // via a body that arrived in the same read as the header terminator.
    let leftover = &buf[sep + 4..];
    if leftover.len() > max_body {
        http_reject_and_close(stream, 413);
        return None;
    }
    // Allocate with bounded capacity (never the unbounded client value): we will
    // read at most `content_length` bytes, itself already <= max_body.
    let mut body = Vec::with_capacity(content_length.min(max_body));
    body.extend_from_slice(leftover);
    while body.len() < content_length {
        // Read no more than what is still wanted; stop hard at the cap.
        let want = content_length - body.len();
        let chunk = want.min(tmp.len());
        match stream.read(&mut tmp[..chunk]) {
            Ok(0) => break,
            Ok(n) => {
                body.extend_from_slice(&tmp[..n]);
                if body.len() > max_body {
                    // Defensive: should be unreachable given the checks above,
                    // but never let the buffer grow past the cap.
                    http_reject_and_close(stream, 413);
                    return None;
                }
            }
            Err(_) => break,
        }
    }
    // Bytes past the body arrived with it. On a persistent connection they are
    // the next request and go back to the caller. The one-shot rule below
    // leaves them in a zero-length body, which is only harmless because that
    // connection is about to close.
    let leftover = if persistent_conn && body.len() > content_length {
        let rest = body[content_length..].to_vec();
        body.truncate(content_length);
        rest
    } else {
        if body.len() > content_length && content_length > 0 {
            body.truncate(content_length);
        }
        Vec::new()
    };
    let persistent = http11 && !request_asks_close(&headers);
    Some(ParsedRequest {
        method,
        uri,
        headers,
        body,
        persistent,
        http11,
        leftover,
    })
}

/// Build a real `com.sun.net.httpserver.Headers` (a `HashMap<String,List<String>>`
/// subclass) and populate it via its real `add` bytecode, so the Java
/// `HttpHandler` sees a fully-functional Map (`entrySet`/`get`/`getFirst` all
/// work). Verified byte-identical to HotSpot incl. header-name normalisation.
pub fn re10_build_headers(
    ctx: &mut dyn NativeContext,
    entries: &[(String, String)],
) -> Result<ObjectRef, cratonvm_types::error::MethodCallFailed> {
    let hdrs0 = match ctx.new_object_initialized("com/sun/net/httpserver/Headers", "()V", &[])? {
        Some(Value::Object(Some(o))) => o,
        _ => return Err(ioex("Headers <init> failed")),
    };
    // GC-safety: `hdrs` is held across `create_string`/`add` allocations for
    // every entry. A moving GC mid-loop would otherwise leave the Rust-local
    // `hdrs` stale and silently drop every subsequent `add` (writes to a vacated
    // slot) — i.e. an incomplete request/response header map under GC pressure
    // (ES testHeaders / auth). Pin `hdrs` for the whole build, and pin each key
    // string across the value-string allocation that follows it; re-read the
    // forwarded addresses before the `add`. The just-created value string `vs`
    // is used immediately (no allocation before the invoke) so it needs no pin.
    let h_pin = ctx.pin_native_root(hdrs0);
    for (k, v) in entries {
        let ks0 = ctx.create_string(k);
        let ks_pin = ctx.pin_native_root(ks0);
        let vs = ctx.create_string(v);
        let hdrs = ctx.read_native_pin(h_pin, hdrs0);
        let ks = ctx.read_native_pin(ks_pin, ks0);
        let _ = ctx.invoke(
            "com/sun/net/httpserver/Headers",
            "add",
            "(Ljava/lang/String;Ljava/lang/String;)V",
            &[
                Value::Object(Some(hdrs)),
                Value::Object(Some(ks)),
                Value::Object(Some(vs)),
            ],
        );
        // Pop just this iteration's key-string pin (keep `hdrs` pinned).
        ctx.unpin_native_roots(ks_pin);
    }
    let hdrs = ctx.read_native_pin(h_pin, hdrs0);
    ctx.unpin_native_roots(h_pin);
    Ok(hdrs)
}

/// Read a real `Map<String,List<String>>` (the response Headers the handler
/// populated) into flat (name, value) pairs via its polymorphic
/// `entrySet().iterator()` — the same layout-agnostic walk the collections
/// crate uses for unmodelled maps.
pub fn re10_read_headers(ctx: &mut dyn NativeContext, map: ObjectRef) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let set = match ctx.invoke(
        "java/util/Map",
        "entrySet",
        "()Ljava/util/Set;",
        &[Value::Object(Some(map))],
    ) {
        Ok(Some(Value::Object(Some(s)))) => s,
        _ => return out,
    };
    let it0 = match ctx.invoke(
        "java/util/Set",
        "iterator",
        "()Ljava/util/Iterator;",
        &[Value::Object(Some(set))],
    ) {
        Ok(Some(Value::Object(Some(i)))) => i,
        _ => return out,
    };
    // GC-safety: the iterator (`it`), each `entry`, and each value `List` are
    // held across the per-element `hasNext`/`next`/`getKey`/`getValue`/`size`/
    // `get` invokes, every one of which allocates. A moving GC mid-walk would
    // leave a Rust-local stale and truncate/garble the read header map (ES
    // testHeaders). Pin the iterator for the whole walk and each entry / value
    // list across the invokes that consume it; the key/value Strings are turned
    // into Rust `String`s immediately (no held ObjectRef). `it_pin` is the base;
    // per-iteration pins are popped before the next round so the pin stack stays
    // bounded.
    let it_pin = ctx.pin_native_root(it0);
    loop {
        let it = ctx.read_native_pin(it_pin, it0);
        let has = matches!(
            ctx.invoke(
                "java/util/Iterator",
                "hasNext",
                "()Z",
                &[Value::Object(Some(it))]
            ),
            Ok(Some(Value::Int(1)))
        );
        if !has {
            break;
        }
        let it = ctx.read_native_pin(it_pin, it0);
        let entry0 = match ctx.invoke(
            "java/util/Iterator",
            "next",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(it))],
        ) {
            Ok(Some(Value::Object(Some(e)))) => e,
            _ => break,
        };
        let entry_pin = ctx.pin_native_root(entry0);
        let key = match ctx.invoke(
            "java/util/Map$Entry",
            "getKey",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry0))],
        ) {
            Ok(Some(Value::Object(Some(k)))) => ctx.read_string(k).unwrap_or_default(),
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        let entry = ctx.read_native_pin(entry_pin, entry0);
        let val_list0 = match ctx.invoke(
            "java/util/Map$Entry",
            "getValue",
            "()Ljava/lang/Object;",
            &[Value::Object(Some(entry))],
        ) {
            Ok(Some(Value::Object(Some(l)))) => l,
            _ => {
                ctx.unpin_native_roots(entry_pin);
                continue;
            }
        };
        // `entry` is done; reuse its pin slot for the value list.
        ctx.unpin_native_roots(entry_pin);
        let vl_pin = ctx.pin_native_root(val_list0);
        let n = match ctx.invoke(
            "java/util/List",
            "size",
            "()I",
            &[Value::Object(Some(val_list0))],
        ) {
            Ok(Some(Value::Int(n))) => n,
            _ => 0,
        };
        for i in 0..n {
            let val_list = ctx.read_native_pin(vl_pin, val_list0);
            if let Ok(Some(Value::Object(Some(s)))) = ctx.invoke(
                "java/util/List",
                "get",
                "(I)Ljava/lang/Object;",
                &[Value::Object(Some(val_list)), Value::Int(i)],
            ) {
                out.push((key.clone(), ctx.read_string(s).unwrap_or_default()));
            }
        }
        ctx.unpin_native_roots(vl_pin);
    }
    ctx.unpin_native_roots(it_pin);
    out
}

/// Slot on the synthetic `HttpExchange` holding the authenticated
/// `HttpPrincipal` (null until an `Authenticator.Success` supplies one).
///
/// Slots 0..=7 are all taken by the dispatcher (method, URI, request headers,
/// response headers, request body, status, response chunks, response-length
/// hint), so the principal takes a ninth. `re10_dispatch_pending` is the ONLY
/// place an `HttpExchange` is allocated, and readers still bounds-check with
/// `object_num_fields` so an exchange minted anywhere else simply reports "no
/// principal" instead of running off the end of the object.
pub const HEX_PRINCIPAL: usize = 8;

/// Slots holding the connection's local and remote endpoints as
/// `java/net/InetSocketAddress` objects, captured from the accepted
/// `TcpStream` in `re10_dispatch_pending` (the one place that still holds it).
/// `HttpExchange.getLocalAddress()` / `getRemoteAddress()` are plain reads of
/// these. Null only if the socket is already torn down when the exchange is
/// minted (`peer_addr()`/`local_addr()` failing) — never fabricated, because a
/// caller that logs or rate-limits by peer must not attribute every request to
/// one invented host.
///
/// `pub` because the two getters that read them are registered in
/// `phases_late::net_channels`, and a hard-coded `9`/`10` over there is exactly
/// the kind of drift this slot map keeps producing.
pub const HEX_LOCAL_ADDR: usize = 9;
pub const HEX_REMOTE_ADDR: usize = 10;

/// The owning `HttpContext`, so `HttpExchange.getHttpContext()` can answer it
/// directly instead of AbstractMethodError-ing: the JDK declares this method
/// abstract on `HttpExchange`, and until this slot existed nothing registered
/// it in ANY mode, real-JDK included. `re10_dispatch_pending` already resolves
/// the context to find the handler, so storing it here is free — no lookup,
/// no link table, unlike `HttpContext.getServer()` (see `HTTP_LINK_CONTEXT_SERVER`
/// in `phases_late::net_channels`), which has no spare slot of its own.
pub const HEX_CONTEXT: usize = 11;

/// The request line's HTTP version (`"HTTP/1.1"` or `"HTTP/1.0"`), for
/// `HttpExchange.getProtocol()` — same missing-registration defect as
/// `HEX_CONTEXT` above, in every mode. Mirrors `PendingRequest.http11`, which
/// the dispatcher already parses off the request line for the keep-alive
/// decision.
pub const HEX_PROTOCOL: usize = 12;

/// Number of slots `re10_dispatch_pending` asks `alloc_concurrent_synthetic`
/// for when it mints an exchange.
///
/// KEEP IN LOCK-STEP with the `"com/sun/net/httpserver/HttpExchange"` entry in
/// `classloading/src/class_manager.rs` `synthetic_stub_fields` — it declares
/// the class's instance-field count, and a class that declares FEWER fields
/// than the index being written makes `set_field` DROP the write silently
/// rather than error. The natives then look implemented while storing nothing;
/// that is exactly how `HEX_PRINCIPAL` sat dead behind an `instance_fields(8)`
/// entry. If you add a slot here, add it there in the SAME change.
pub const HEX_NUM_FIELDS: usize = 13;

/// The three `Authenticator.Result` subclasses the `com.sun.net.httpserver`
/// contract defines. None of them is `final` in the JDK, so results are
/// classified by walking the receiver's ancestry rather than by an exact name
/// match.
pub enum AuthResultKind {
    Success,
    Retry,
    Failure,
}

/// Verdict of the per-context authentication gate.
pub enum AuthGate {
    /// Run the handler: either the context has no authenticator at all, or the
    /// authenticator returned `Success` (and its principal is now on the
    /// exchange).
    Proceed,
    /// Answer the client with this status and an empty body; the handler is not
    /// invoked. Covers `Retry`, `Failure`, and every way the authenticator can
    /// fail to produce a usable verdict.
    Reject(i32),
}

pub fn re10_auth_result_kind(ctx: &dyn NativeContext, result: ObjectRef) -> Option<AuthResultKind> {
    let mut cid = ctx.class_id_of_object(result);
    // Bounded walk: `Result` sits one or two links above the concrete subclass,
    // and the bound keeps a pathological/self-referential chain from spinning.
    for _ in 0..16 {
        match ctx.class_name_of_id(cid)?.as_str() {
            "com/sun/net/httpserver/Authenticator$Success" => return Some(AuthResultKind::Success),
            "com/sun/net/httpserver/Authenticator$Retry" => return Some(AuthResultKind::Retry),
            "com/sun/net/httpserver/Authenticator$Failure" => return Some(AuthResultKind::Failure),
            _ => {}        }
        cid = ctx.superclass_of(cid)?;    }
    None
}

/// Run the `HttpContext`'s `Authenticator` (if any) against `ex0`.
///
/// `ex_pin`/`ex0` are the caller's pin handle and original address for the
/// exchange: authenticating runs arbitrary Java bytecode (`BasicAuthenticator`
/// allocates strings, reads the request headers and writes `WWW-Authenticate`
/// into the response headers), so every use of the exchange here re-reads the
/// forwarded address instead of trusting a Rust local.
///
/// Nothing thrown by the authenticator is allowed to escape: this runs on the
/// dispatcher thread's accept loop, where an error would abandon the connection
/// and take the request pump down with it. A throw is a 500, exactly as the
/// `HttpHandler.handle` backstop treats a broken handler.
pub fn re10_authenticate(
    ctx: &mut dyn NativeContext,
    hctx: ObjectRef,
    ex_pin: usize,
    ex0: ObjectRef,
) -> Result<AuthGate, MethodCallFailed> {
    // Cheap probe first. `HttpContext.getAuthenticator` is a registered native
    // that only looks the context up in a side table, so the no-authenticator
    // path costs one native call and allocates nothing.
    let auth0 = match ctx.invoke_virtual(
        hctx,
        "getAuthenticator",
        "()Lcom/sun/net/httpserver/Authenticator;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(a)))) => a,
        _ => return Ok(AuthGate::Proceed),
    };
    // Pinned from here on; `unpin_native_roots(auth_pin)` at the end releases
    // this and the result (both pinned after the caller's batch, so the
    // caller's handler/context/exchange pins survive).
    let auth_pin = ctx.pin_native_root(auth0);
    let ex = ctx.read_native_pin(ex_pin, ex0);
    let auth = ctx.read_native_pin(auth_pin, auth0);
    let result0 = match ctx.invoke_virtual(
        auth,
        "authenticate",
        "(Lcom/sun/net/httpserver/HttpExchange;)Lcom/sun/net/httpserver/Authenticator$Result;",
        &[Value::Object(Some(ex))],
    ) {
        Ok(Some(Value::Object(Some(r)))) => r,
        // A throw, or a null `Result` (which would NPE inside the JDK's own
        // AuthFilter) — either way the request is NOT authenticated.
        _ => {
            ctx.unpin_native_roots(auth_pin);
            return Ok(AuthGate::Reject(500));
        }
    };
    let result_pin = ctx.pin_native_root(result0);
    // Classified before the match so the shared borrow of `ctx` it needs cannot
    // overlap the mutable borrows the arms take.
    let kind = re10_auth_result_kind(&*ctx, result0);
    let gate = match kind {
        Some(AuthResultKind::Success) => {
            let result = ctx.read_native_pin(result_pin, result0);
            let principal = match ctx.invoke_virtual(
                result,
                "getPrincipal",
                "()Lcom/sun/net/httpserver/HttpPrincipal;",
                &[],
            ) {
                Ok(Some(p @ Value::Object(Some(_)))) => p,
                _ => Value::Object(None),
            };
            // Nothing allocates between reading the principal and storing it,
            // so the value itself needs no pin; the exchange does.
            let ex = ctx.read_native_pin(ex_pin, ex0);
            if ctx.object_num_fields(ex) > HEX_PRINCIPAL {
                ctx.set_field(ex, HEX_PRINCIPAL, principal);
            }
            AuthGate::Proceed
        }
        Some(AuthResultKind::Retry) | Some(AuthResultKind::Failure) => {
            let result = ctx.read_native_pin(result_pin, result0);
            // Both subclasses expose the status the same way. 401 is the
            // fallback the JDK's own BasicAuthenticator would have produced if
            // the accessor is unreadable.
            let code = match ctx.invoke_virtual(result, "getResponseCode", "()I", &[]) {
                Ok(Some(v)) => v.as_int().unwrap_or(401),
                _ => 401,
            };
            AuthGate::Reject(code)
        }
        // A `Result` that is none of the three documented subclasses is not a
        // pass — the JDK filter would fall through without answering at all,
        // which here would silently serve the request unauthenticated.
        None => AuthGate::Reject(500),
    };
    ctx.unpin_native_roots(auth_pin);
    Ok(gate)
}

pub fn re10_dispatch_pending(
    ctx: &mut dyn NativeContext,
    server_id: i32,
) -> Result<usize, cratonvm_types::error::MethodCallFailed> {
    let mut drained = 0usize;
    loop {
        let req_opt = {
            let mut q = request_queue().lock();
            // FIFO. `pop()` served the NEWEST request first, so under a burst
            // the oldest waiter was the last one answered.
            q.get_mut(&server_id).and_then(|v| {
                if v.is_empty() {
                    None
                } else {
                    Some(v.remove(0))
                }
            })
        };
        let Some(req) = req_opt else { break };
        drained += 1;
        let handler_info = {
            let state = {
                let reg = server_registry().lock();
                reg.get(&server_id).cloned()
            };
            state.and_then(|s| {
                let hs = s.handlers.lock();
                hs.iter()
                    .filter(|e| req.uri.starts_with(&e.path_prefix))
                    .max_by_key(|e| e.path_prefix.len())
                    .map(|e| (e.handler, e.context))
            })
        };
        let (status, body_bytes, resp_headers, len_hint) = match handler_info {
            Some((h, hctx0)) => {
                // GC-safety: this native dispatcher holds the handler (`h`) and
                // the HttpExchange (`ex`) across many VM allocations below
                // (create_string, build-headers, byte arrays, ref array, and the
                // handler invoke). A bare ObjectRef in a Rust local does NOT
                // survive a moving GC those allocations may trigger — it goes
                // stale, which surfaced as a `NoSuchMethodError
                // java/lang/Object.handle` storm (stale receiver) and corrupted
                // exchange field writes (landing in a vacated from-space slot)
                // under -Xmx1g GC pressure (ES testManyAsyncRequests /
                // testHeaders). Pin both as native roots and re-read the
                // forwarded address (`read_native_pin`) after every allocation.
                // Each freshly-created field VALUE is set immediately (no
                // allocation between its create and its set), so values need no
                // pin. `unpin_native_roots(h_pin)` releases the whole batch.
                let h_pin = ctx.pin_native_root(h);
                // The owning HttpContext rides along in the same pin batch: the
                // authentication gate below dereferences it after every one of
                // those allocations.
                let hctx_pin = ctx.pin_native_root(hctx0);
                let ex0 = try_alloc_concurrent_synthetic(
                    ctx,
                    "com/sun/net/httpserver/HttpExchange",
                    HEX_NUM_FIELDS,
                )?;
                let ex_pin = ctx.pin_native_root(ex0);

                let m = ctx.create_string(&req.method);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 0, Value::Object(Some(m)));
                let u = ctx.create_string(&req.uri);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 1, Value::Object(Some(u)));
                // Request + response headers are REAL `Headers` (HashMap subclass)
                // objects so the handler's `entrySet()`/`put()`/`getFirst()` run
                // real bytecode. The synthetic Headers had no Map methods.
                let rh = match re10_build_headers(ctx, &req.headers) {
                    Ok(r) => r,
                    Err(e) => {
                        ctx.unpin_native_roots(h_pin);
                        return Err(e);
                    }
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 2, Value::Object(Some(rh)));
                let rsph = match re10_build_headers(ctx, &[]) {
                    Ok(r) => r,
                    Err(e) => {
                        ctx.unpin_native_roots(h_pin);
                        return Err(e);
                    }
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 3, Value::Object(Some(rsph)));
                let body_arr = new_java_byte_array(ctx, &req.body);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 4, Value::Object(Some(body_arr)));
                ctx.set_field(ex, 5, Value::Int(200));
                let resp_body_chunks = ctx.new_ref_array(ClassId::new(0), 64);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, 6, Value::Object(Some(resp_body_chunks)));
                ctx.set_field(ex, 7, Value::Int(0));
                // No principal until an `Authenticator.Success` supplies one;
                // `HttpExchange.getPrincipal` reads this slot.
                ctx.set_field(ex, HEX_PRINCIPAL, Value::Object(None));

                // Connection endpoints. This is the last point at which the
                // accepted `TcpStream` and the exchange coexist (`req.stream`
                // is moved into the responder at the bottom of the loop), so
                // the addresses must be captured here or not at all. Both are
                // built as fully-resolved `InetSocketAddress`es the same way
                // `HttpServer.getAddress()` builds its bound-address echo: the
                // host string is the IP literal, matching the real
                // `com.sun.net.httpserver`, whose exchange addresses come
                // straight off the socket and are never reverse-resolved.
                //
                // GC discipline: `alloc_inet_socket_address_resolved` allocates
                // (holder + InetAddress + strings), so `ex` is re-read from its
                // pin after each one, and each freshly-built address is stored
                // immediately (no allocation between its creation and its
                // `set_field`) so the address itself needs no pin — the same
                // rule the field writes above follow. Storing local first also
                // makes it reachable from the pinned exchange before the remote
                // allocation can move it.
                let local_sa = req.local;
                let remote_sa = req.peer;
                let local_val = match local_sa {
                    Some(sa) => {
                        let ip = sa.ip().to_string();
                        let isa =
                            alloc_inet_socket_address_resolved(ctx, &ip, &ip, sa.port() as i32);
                        Value::Object(Some(isa?))
                    }
                    None => Value::Object(None),
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, HEX_LOCAL_ADDR, local_val);
                let remote_val = match remote_sa {
                    Some(sa) => {
                        let ip = sa.ip().to_string();
                        let isa =
                            alloc_inet_socket_address_resolved(ctx, &ip, &ip, sa.port() as i32);
                        Value::Object(Some(isa?))
                    }
                    None => Value::Object(None),
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, HEX_REMOTE_ADDR, remote_val);

                // Owning context and request protocol, for `getHttpContext()`
                // and `getProtocol()`. `hctx0` is already pinned in this batch
                // (see `hctx_pin` above) and needs no fresh allocation; the
                // protocol string does, so `ex` is re-read after it, same
                // discipline as every field write above.
                let hctx = ctx.read_native_pin(hctx_pin, hctx0);
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, HEX_CONTEXT, Value::Object(Some(hctx)));
                let proto_s = ctx.create_string(if req.http11 { "HTTP/1.1" } else { "HTTP/1.0" });
                let ex = ctx.read_native_pin(ex_pin, ex0);
                ctx.set_field(ex, HEX_PROTOCOL, Value::Object(Some(proto_s)));

                // com.sun.net.httpserver contract: a context with an
                // Authenticator attached authenticates BEFORE the handler runs,
                // and a Retry/Failure result answers the client itself without
                // ever reaching the handler. `re10_authenticate` short-circuits
                // to `Proceed` (one native getter call, no allocation) when the
                // context has no authenticator, which is the overwhelmingly
                // common case.
                let hctx = ctx.read_native_pin(hctx_pin, hctx0);
                let gate = re10_authenticate(ctx, hctx, ex_pin, ex0);
                match gate? {
                    AuthGate::Proceed => {
                        // Re-read both pinned roots immediately before the invoke
                        // (the exchange-build allocations above, and any
                        // authenticator bytecode, may have relocated them).
                        let h = ctx.read_native_pin(h_pin, h);
                        let ex = ctx.read_native_pin(ex_pin, ex0);
                        let _ = ctx.invoke_virtual(
                            h,
                            "handle",
                            "(Lcom/sun/net/httpserver/HttpExchange;)V",
                            &[Value::Object(Some(ex))],
                        );
                    }
                    AuthGate::Reject(code) => {
                        // Same idiom as the `HttpHandler.handle` backstop in
                        // `phases_late::net_channels`: report the status through
                        // the exchange's own natives (`-1` = no response body)
                        // and close it, so the serialization tail below emits it
                        // exactly like any handler-produced response.
                        let ex = ctx.read_native_pin(ex_pin, ex0);
                        let _ = ctx.invoke_virtual(
                            ex,
                            "sendResponseHeaders",
                            "(IJ)V",
                            &[Value::Int(code), Value::Long(-1)],
                        );
                        //
                        // KEPT SWALLOW. Same reasoning as that backstop — no
                        // JDK body to copy a `catch` from — plus one this site
                        // adds: it sits inside the pending-exchange dispatch
                        // LOOP, holding `ex_pin`. Propagating would abort
                        // serving every remaining pending exchange because one
                        // rejected request failed to close, which is a worse
                        // defect than the one being removed. The `Error`
                        // residual is recorded rather than traded for that.
                        // W7-57-close-flush-swallow-sweep.md
                        let ex = ctx.read_native_pin(ex_pin, ex0);
                        let _ = ctx.invoke_virtual(ex, "close", "()V", &[]);
                    }
                }
                // The handler ran bytecode (allocations) — refresh `ex` before
                // reading its populated result fields.
                let ex = ctx.read_native_pin(ex_pin, ex0);
                let status = ctx.get_field(ex, 5).as_int().unwrap_or(200);
                let mut body_bytes: Vec<u8> = Vec::new();
                // W7-24: index of the real-JDK response buffer, if the strict
                // policy refused the `ResponseBody` stand-in and
                // `re10_real_jdk_response_body` parked a
                // `java.io.ByteArrayOutputStream` here instead of chunks.
                // Draining it means an `invoke_virtual`, which allocates, so it
                // happens AFTER this walk rather than inside it — the walk's own
                // invariant is that nothing in it allocates.
                let mut real_sink: Option<usize> = None;
                if let Value::Object(Some(chunks)) = ctx.get_field(ex, 6) {
                    // array_length / get_array_element do not allocate, so the
                    // chunk array and each `ba` stay valid through the walk.
                    let n = ctx.array_length(chunks);
                    for i in 0..n {
                        if let Value::Object(Some(ba)) = ctx.get_array_element(chunks, i) {
                            // `object_is_array` is a heap object-kind check, not
                            // a class-name test — a reference array reports its
                            // COMPONENT class, so a name test cannot tell these
                            // apart. Compatible mode never parks a non-array
                            // here, so this arm is strict-only and the two can
                            // never interleave: whether the mint is refused is a
                            // property of the run, not of the call.
                            if !ctx.object_is_array(ba) {
                                real_sink.get_or_insert(i);
                                continue;
                            }
                            let ln = ctx.array_length(ba);
                            for j in 0..ln {
                                if let Value::Int(b) = ctx.get_array_element(ba, j) {
                                    body_bytes.push(b as i8 as u8);
                                }
                            }
                        }
                    }
                }
                if let Some(i) = real_sink {
                    // Re-read the exchange AND its chunk array from the pin
                    // rather than reusing the locals above: `toByteArray()` runs
                    // a method and can move both.
                    let ex = ctx.read_native_pin(ex_pin, ex0);
                    if let Value::Object(Some(chunks)) = ctx.get_field(ex, 6) {
                        if let Value::Object(Some(sink)) = ctx.get_array_element(chunks, i) {
                            if let Ok(Some(Value::Object(Some(arr)))) =
                                ctx.invoke_virtual(sink, "toByteArray", "()[B", &[])
                            {
                                // `arr` is the invoke's own result, so it is
                                // current, and nothing below allocates.
                                let ln = ctx.array_length(arr);
                                for j in 0..ln {
                                    if let Value::Int(b) = ctx.get_array_element(arr, j) {
                                        body_bytes.push(b as i8 as u8);
                                    }
                                }
                            }
                        }
                    }
                }
                let ex = ctx.read_native_pin(ex_pin, ex0);
                let resp_headers = match ctx.get_field(ex, 3) {
                    Value::Object(Some(rh)) => re10_read_headers(ctx, rh),
                    _ => Vec::new(),
                };
                let ex = ctx.read_native_pin(ex_pin, ex0);
                // Response-length hint from sendResponseHeaders: -1 => no body /
                // no Content-Length (HEAD, 204, 304).
                let len_hint = ctx.get_field(ex, 7).as_int().unwrap_or(0);
                ctx.unpin_native_roots(h_pin);
                (status, body_bytes, resp_headers, len_hint)
            }
            None => (404, b"Not Found".to_vec(), Vec::new(), 0),
        };
        // Keep the connection only when it is a kept-alive connection AND the
        // request allows it. Every response here is framed by Content-length
        // (or is a HEAD, which has no body), so persistence is always safe.
        let keep = req.reply.is_some() && req.persistent;
        let mut resp = Vec::with_capacity(128 + body_bytes.len());
        use std::io::Write as _;
        let _ = write!(&mut resp, "HTTP/1.1 {status} {}\r\n", http_reason(status));
        for (k, v) in &resp_headers {
            // The server owns the message-framing headers (Content-Length,
            // Connection, Transfer-Encoding) and the Date header; the real
            // com.sun.net.httpserver emits exactly one of each and ignores any
            // handler-echoed copy. A handler that copies request headers into the
            // response (as the ES ResponseHandler does) would otherwise produce a
            // DUPLICATE Connection/Content-Length, which the client surfaces as an
            // extra header (ES testHeaders). Skip those here and emit our own.
            if k.eq_ignore_ascii_case("content-length")
                || k.eq_ignore_ascii_case("connection")
                || k.eq_ignore_ascii_case("transfer-encoding")
                || k.eq_ignore_ascii_case("date")
            {
                continue;
            }
            let _ = write!(&mut resp, "{k}: {v}\r\n");
        }
        // Server-controlled framing headers, each exactly once (matches the real
        // com.sun.net.httpserver, which auto-adds Date + Content-length).
        let _ = write!(&mut resp, "Date: {}\r\n", http_date_now());
        // A HEAD response carries no body and (per the real server / ES
        // testHeaders) no Content-Length. Every other method gets a
        // Content-Length — including 0 for an empty body — and the body bytes.
        // (`len_hint` from sendResponseHeaders is captured but the HEAD method is
        // the distinction the client/test actually keys on.)
        let _ = len_hint;
        let is_head = req.method.eq_ignore_ascii_case("HEAD");
        if !is_head {
            // Casing matters: the real com.sun.net.httpserver emits "Content-length"
            // (lowercase 'l') and ES assertHeaders compares header names
            // case-sensitively against that exact spelling.
            let _ = write!(&mut resp, "Content-length: {}\r\n", body_bytes.len());
        }
        // A persistent response carries no Connection header at all, as the
        // real `ServerImpl` sends it (measured: `(none)` on HotSpot).
        // Which closing responses say so, measured on HotSpot 25.0.3
        // (`probes/HttpServerKeepAliveMatrixProbe.java`): an HTTP/1.0 exchange
        // gets `Connection: close`; an HTTP/1.1 request that itself asked for
        // `Connection: close` gets NO Connection header and is simply closed; a
        // persistent response gets none. The one-shot path (no `reply`) keeps
        // announcing the close it always performs.
        if keep || (req.reply.is_some() && req.http11) {
            resp.extend_from_slice(b"\r\n");
        } else {
            resp.extend_from_slice(b"Connection: close\r\n\r\n");
        }
        if !is_head {
            resp.extend_from_slice(&body_bytes);
        }
        match (req.reply, req.stream) {
            // Kept-alive: the connection's own thread writes it, then either
            // parses the next request or closes (see `re10_serve_connection`).
            (Some(reply), _) => {
                let _ = reply.send(resp);
            }
            // One-shot: write the response and close on a short-lived I/O thread
            // (pure socket work, no VM context needed) so the dispatcher returns
            // immediately to serve the next queued request instead of blocking
            // on the per-connection lingering close.
            (None, Some(stream)) => re10_send_response(stream, resp),
            (None, None) => {}
        }
    }
    Ok(drained)
}

/// Write a fully-formed HTTP response to `stream` and close it gracefully on a
/// detached thread. The close is a *lingering* close: send FIN (`shutdown(Write)`)
/// then drain the read side until the peer closes (EOF) before dropping the
/// socket. This avoids the Windows RST-on-close that resets a peer still reading
/// the response (a non-blocking Apache NIO reactor surfaced this as
/// "connection reset" / os error 10053); a bare `shutdown(Both)` discards unread
/// bytes and is RST-prone, while no drain at all races the OS close against the
/// client's read.
pub fn re10_send_response(mut stream: TcpStream, resp: Vec<u8>) {
    let _ = std::thread::Builder::new()
        .name("cratonvm-httpserver-resp".to_string())
        .spawn(move || {
            let _ = stream.write_all(&resp);
            let _ = stream.flush();
            re10_lingering_close(stream);
        });
}

/// The graceful close both server paths end a connection with: FIN, then drain
/// the read side until the peer closes, so Windows does not RST a peer still
/// reading the response (see `re10_send_response`).
pub fn re10_lingering_close(mut stream: TcpStream) {
    let _ = stream.shutdown(std::net::Shutdown::Write);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut drain = [0u8; 512];
    loop {
        match stream.read(&mut drain) {
            Ok(0) => break,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
}

/// Serve one accepted connection for as long as it persists.
///
/// This is what makes the embedded server keep connections alive. It used to
/// answer every request with `Connection: close`: measured with 1,500
/// sequential requests, the server saw 1,500 distinct client ports where
/// HotSpot's saw 1, and both JDK client stacks sat at ~2.5 ms per request
/// against HotSpot's 0.1-0.3 ms. The connection owns this thread: it parses a
/// request, hands it to the dispatchers, waits for the serialised response,
/// writes it, and loops, carrying over any bytes that arrived past the request.
///
/// It ends on the peer closing, the idle interval passing with no new request,
/// a request asking for `Connection: close` (lingering close, as the one-shot
/// path does), a malformed request (already answered by the parser), or
/// `stop()` shutting the socket.
pub fn re10_serve_connection(
    server_id: i32,
    state: std::sync::Arc<ServerState>,
    mut stream: TcpStream,
) {
    let local = stream.local_addr().ok();
    let peer = stream.peer_addr().ok();
    let conn_id = state.next_conn.fetch_add(1, Ordering::Relaxed);
    if let Ok(handle) = stream.try_clone() {
        state.conns.lock().insert(conn_id, handle);
    }
    let mut carry = Vec::new();
    let mut between_requests = false;
    let lingering = loop {
        if !state.running.load(Ordering::SeqCst) {
            break false;
        }
        let Some(parsed) = parse_http_request_core(
            &mut stream,
            std::mem::take(&mut carry),
            between_requests,
            true,
        ) else {
            break false;
        };
        between_requests = true;
        carry = parsed.leftover;
        let persistent = parsed.persistent;
        let (reply, response) = std::sync::mpsc::channel::<Vec<u8>>();
        re10_enqueue(
            server_id,
            &state,
            PendingRequest {
                stream: None,
                local,
                peer,
                method: parsed.method,
                uri: parsed.uri,
                headers: parsed.headers,
                body: parsed.body,
                reply: Some(reply),
                persistent,
                http11: parsed.http11,
            },
            true,
        );
        let Ok(bytes) = response.recv() else {
            break false;
        };
        if stream
            .write_all(&bytes)
            .and_then(|()| stream.flush())
            .is_err()
        {
            break false;
        }
        if !persistent {
            break true;
        }
    };
    state.conns.lock().remove(&conn_id);
    if lingering {
        re10_lingering_close(stream);
    }
}

/// Synthetic Runnable whose `run()` drives the per-server HTTP dispatch loop on
/// a real VM thread. Field 0 holds the `server_id`.
pub const HS_LOOP_CLASS: &str = "CratonVM$HttpServerLoop";

// Requested field count for the worker `java/lang/Thread`. The VM may hand back
// the real-JDK layout (more fields) instead — `Thread.<init>` handles both.
pub const HS_THREAD_NUM_FIELDS: usize = 5;

/// Build a daemon VM thread whose `run()` is `re10_serve_loop_run` for
/// `server_id`, and start it via the VM's real thread machinery. The thread
/// exits when the server's `running` flag clears (stop()).
/// Number of VM dispatcher threads draining the request queue concurrently.
/// More than one lets independent requests run their Java handlers in parallel
/// (each request is popped by exactly one thread), which is what keeps a burst
/// of hundreds of concurrent requests (ES testManyAsyncRequests) inside the
/// client's timeout instead of serializing every handler on one thread.
pub const HS_DISPATCHER_POOL: i32 = 4;

pub fn re10_spawn_dispatcher(
    ctx: &mut dyn NativeContext,
    server_id: i32,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let dbg = crate::nbflags().dbg_httpsrv;
    for idx in 0..HS_DISPATCHER_POOL {
        // W7-24 — `ensure_vm_internal_class`, not the compatibility door.
        //
        // `CratonVM$HttpServerLoop` is a shape this VM invents to carry
        // `server_id` from here to `re10_serve_loop_run` on a real VM thread.
        // `javap CratonVM.HttpServerLoop` against the JDK 25 image answers
        // "class not found", the name is in no JDK namespace at all, and no
        // class file can ever back it — contract §1 item 6's shape, which
        // `ensure_vm_internal_class` exists to mint in every mode. Through
        // `try_alloc_concurrent_synthetic` alone it acquired
        // `ClassOrigin::CompatibilityStub`, which `--real-jdk` correctly
        // refuses, and the refusal killed the whole server:
        //
        //     $ cratonvm --real-jdk … HttpServerWildcardAddressProbe
        //     NoClassDefFoundError: CratonVM$HttpServerLoop
        //         at HttpServerWildcardAddressProbe.main(…:49)
        //
        // i.e. `com.sun.net.httpserver.HttpServer.start()` could not start
        // under strict mode. `bind()` and `getAddress()` were already fine —
        // this is the one call the door took out.
        //
        // The natives are the OTHER gate, and here it is already open,
        // measured rather than assumed: `--dump-native-registry` taken once
        // per mode reports this class's single `run()V` as `kind":"bridge"` in
        // Compatible AND under `--real-jdk`. The name is on none of
        // `no_image_receiver.rs`'s tables — it does not start with `cratonvm/`
        // so `VM_MINTED_STAND_IN_RECEIVERS` is not consulted, and it is in
        // neither `NO_IMAGE_JDK_RECEIVERS` nor `VM_SERVICE_RECEIVERS` — so
        // `receiver_declared_by_no_supported_image` answers false and nothing
        // re-tags it `SyntheticStub`. That check is per class and is not a
        // general licence: flipping the door on a receiver whose natives ARE
        // dropped in strict buys an `UnsatisfiedLinkError` at the first call
        // instead of a `NoClassDefFoundError` at the mint, which is a moved
        // symptom rather than a fix (measured on
        // `cratonvm/internal/LinkedListSnapshotListItr`).
        //
        // Pre-mint rather than replace, so the allocation below keeps its
        // real-vs-requested field-count widening and its GC-safe retry:
        // `fabricate_class` returns the existing `ClassId` for an
        // already-loaded name before it reaches `admit_compatibility_class`,
        // so `Compatible` is byte-for-byte unchanged and only the recorded
        // ORIGIN moves (`compatibility-stub` → `vm-internal` in
        // `--dump-class-origins`, and `vm_exec.rs`'s `stub_hint` stops
        // suggesting a missing jar for a class no jar has).
        ctx.ensure_vm_internal_class(HS_LOOP_CLASS, 1);
        let runner = try_alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1)?;
        ctx.set_field(runner, 0, Value::Int(server_id));

        let worker = try_alloc_concurrent_synthetic(ctx, "java/lang/Thread", HS_THREAD_NUM_FIELDS)?;
        let name = ctx.create_string(&format!("cratonvm-httpserver-dispatch-{server_id}-{idx}"));
        // Populate the worker Thread via the registered
        // `Thread.<init>(ThreadGroup, Runnable, String)` native. This stores the
        // runnable the right way for BOTH layouts: slot 3 (`target`) on a
        // synthetic <=8-field Thread, or `holder:FieldHolder.task` on a real-JDK
        // Thread — which is what `Thread.run()` actually reads. Setting `target`
        // by name on a real-JDK Thread does NOT work (no top-level `target`
        // field; the runnable lives in the FieldHolder), which is why the loop
        // never started before.
        let _ = ctx.invoke(
            "java/lang/Thread",
            "<init>",
            "(Ljava/lang/ThreadGroup;Ljava/lang/Runnable;Ljava/lang/String;)V",
            &[
                Value::Object(Some(worker)),
                Value::Object(None),
                Value::Object(Some(runner)),
                Value::Object(Some(name)),
            ],
        );
        // Daemon so a server left unstopped never wedges VM shutdown after main()
        // returns (it normally exits on stop() when `running` clears).
        let _ = ctx.invoke(
            "java/lang/Thread",
            "setDaemon",
            "(Z)V",
            &[Value::Object(Some(worker)), Value::Int(1)],
        );
        // Best-effort: if the VM has no thread registry (e.g. test mocks) the
        // start is a no-op; start()/stop() still drain the queue as a fallback.
        let res = ctx.thread_start(worker);
        if dbg {
            eprintln!(
                "[HTTPSRV] spawn_dispatcher server={server_id} idx={idx} num_fields={} thread_start_ok={}",
                ctx.object_num_fields(worker),
                res.is_ok()
            );
        }
    }
    Ok(())
}

/// `CratonVM$HttpServerLoop.run()` — runs on a dedicated VM thread. Drains the
/// inbound request queue and dispatches each request through the real Java
/// `HttpHandler`, then idles (in a GC-blocked region) until more arrive. Exits
/// when the server's `running` flag clears.
pub fn re10_serve_loop_run(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let server_id = ctx.get_field(this, 0).as_int().unwrap_or(-1);
    let dbg = crate::nbflags().dbg_httpsrv;
    if dbg {
        eprintln!("[HTTPSRV] serve_loop ENTER server={server_id}");
    }
    if server_id < 0 {
        return Ok(None);
    }
    loop {
        let running = server_registry()
            .lock()
            .get(&server_id)
            .map(|s| s.running.load(Ordering::SeqCst))
            .unwrap_or(false);
        if !running {
            if dbg {
                eprintln!("[HTTPSRV] serve_loop EXIT server={server_id} (not running)");
            }
            break;
        }
        // Dispatch runs Java bytecode (the handler) which cooperates with
        // safepoints normally — only the idle wait needs a blocking region.
        let drained = re10_dispatch_pending(ctx, server_id)?;
        if drained == 0 {
            // `InNative` (interpreter round i3 wave 82, lane L6): the JDK
            // `HttpServer` dispatcher this loop stands for idles in
            // `Selector.select` -- RUNNABLE, in native, on HotSpot.
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            if cratonvm_native_api::net_wait::event_waits_enabled() {
                // Wake on the enqueue itself. The 2 ms sleep this replaces was
                // paid by EVERY request of a sequential client, which always
                // arrives to an empty queue. The bound only keeps `running`
                // re-checked; `stop()` also notifies.
                let mut q = request_queue().lock();
                if q.get(&server_id).map_or(true, |v| v.is_empty()) {
                    let _ = request_queue_ready().wait_for(&mut q, Duration::from_millis(50));
                }
            } else {
                std::thread::sleep(Duration::from_millis(2));
            }
            ctx.end_blocking_region();
        }
    }
    Ok(None)
}

/// Current time as an RFC 1123 HTTP-date (e.g. "Thu, 18 Jun 2026 08:37:05 GMT").
/// The real `com.sun.net.httpserver` adds a `Date` response header automatically;
/// clients (and the ES `testHeaders` assertion) expect it.
pub fn http_date_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_http_date(secs)
}

/// Format epoch seconds as an RFC 1123 date in GMT (civil-from-days per
/// Howard Hinnant's algorithm; no external date crate).
pub fn format_http_date(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let rem = epoch_secs % 86400;
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // 1970-01-01 is a Thursday; Sun=0.
    let dow = (((days % 7) + 4) % 7) as usize;
    let dow_name = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][dow];
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    let mon_name = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][(m - 1) as usize];
    format!("{dow_name}, {d:02} {mon_name} {year} {hh:02}:{mm:02}:{ss:02} GMT")
}

pub fn http_reason(code: i32) -> &'static str {
    match code {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        // VULN-FIX [nb-net-phase-e]: 413 used to reject oversized request bodies
        // (Content-Length exceeding the configurable max — see http_max_request_body()).
        413 => "Payload Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "OK",
    }
}

pub fn re10_start_server(server_id: i32) -> std::io::Result<()> {
    let state = {
        let reg = server_registry().lock();
        reg.get(&server_id).cloned()
    };
    let Some(state) = state else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "server not registered",
        ));
    };
    if state.running.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    if state.listener.lock().is_none() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotConnected,
            "not bound",
        ));
    }
    let state_cl = state.clone();
    std::thread::Builder::new()
        .name(format!("cratonvm-httpserver-{server_id}"))
        .spawn(move || {
            while state_cl.running.load(Ordering::SeqCst) {
                // Hold the listener through the bounded wait so stop can close it
                // synchronously after the kernel releases its reference.
                let accepted = {
                    let guard = state_cl.listener.lock();
                    let Some(listener) = guard.as_ref() else {
                        break;
                    };
                    let accepted = listener.accept();
                    if accepted
                        .as_ref()
                        .is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock)
                        && cratonvm_native_api::net_wait::event_waits_enabled()
                    {
                        // Keep the listener locked until poll releases its OS reference.
                        cratonvm_native_api::net_wait::wait_readable_raw(
                            cratonvm_native_api::net_wait::raw_sock(listener),
                            50,
                        );
                    }
                    accepted
                };
                match accepted {
                    Ok((stream, _peer)) => {
                        if !state_cl.running.load(Ordering::SeqCst) {
                            drop(stream);
                            break;
                        }
                        // Parse each connection on its own short-lived thread so a
                        // slow (or merely not-yet-written) request never blocks the
                        // accept loop. Serially parsing here let the OS listen
                        // backlog overflow under burst load (hundreds of concurrent
                        // Connection: close requests), failing requests — see ES
                        // testManyAsyncRequests. Parse is pure socket work and needs
                        // no VM context; the dispatcher thread(s) run the handler.
                        let sid = server_id;
                        let conn_state = state_cl.clone();
                        let _ = std::thread::Builder::new()
                            .name(format!("cratonvm-httpserver-parse-{sid}"))
                            .spawn(move || {
                                if httpsrv_keepalive_enabled() {
                                    re10_serve_connection(sid, conn_state, stream);
                                } else if let Some(pending) = parse_http_request(stream) {
                                    re10_enqueue(sid, &conn_state, pending, false);
                                }
                            });
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        // The event wait already ran under the listener lock.
                        if !cratonvm_native_api::net_wait::event_waits_enabled() {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                    }
                    Err(_) => break,
                }
            }
        })?;
    Ok(())
}

/// Open (and configure) the listening socket for an `InetSocketAddress`.
///
/// Returns the live listener plus the endpoint it ACTUALLY landed on: port 0
/// means "ephemeral", and `getAddress()` has to report the port the OS picked,
/// not the zero the caller asked for.
pub fn re10_open_listener(
    ctx: &mut dyn NativeContext,
    sa: ObjectRef,
    backlog: i32,
) -> Result<(TcpListener, IpAddr, i32), MethodCallFailed> {
    let (host, port) = read_inet_socket_address(ctx, sa)?;
    let ip = resolve_host(&host)?;
    let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
    // `ServerImpl` binds with the caller's backlog (`< 1` meaning 50). This was
    // `TcpListener::bind`, at std's 128 whatever `create(addr, backlog)` said.
    let listener = cratonvm_native_api::net_wait::bind_tcp_listener(addr, backlog)
        .map_err(|e| ioex(format!("HttpServer bind {addr}: {e}")))?;
    // Non-blocking so the accept loop polls `running` (and so it can be
    // closed promptly by stop()).
    listener.set_nonblocking(true).ok();
    let bound_addr = listener.local_addr().ok();
    let bound_port = bound_addr.map(|a| a.port() as i32).unwrap_or(port);
    let bound_ip = bound_addr.map(|a| a.ip()).unwrap_or(ip);
    Ok((listener, bound_ip, bound_port))
}

/// Allocate the native-backed `HttpServer` receiver AND its `server_registry`
/// entry, as one operation.
///
/// Every `HttpServer` factory MUST come through here. `bind`/`start`/`stop`/
/// `createContext` all locate their state by the `HS_SERVER_ID` slot, so a
/// factory that mints a receiver without a registry entry produces a server
/// that can never be started: the phase-72 no-arg `create()` used to allocate
/// its own 3-slot object with no id and no entry, and `start()` on it read slot
/// `HS_SERVER_ID` out of bounds (id 0, which is never handed out — the counter
/// starts at 1) and failed with `IOException: server not registered`.
///
/// `bound` is `None` for the UNBOUND servers `HttpServer.create()` and
/// `create(null, backlog)` return; the JDK contract is that such a server must
/// be `bind()`-ed before `start()`.
pub fn re10_alloc_server(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    bound: Option<(TcpListener, IpAddr, i32)>,
) -> Result<ObjectRef, MethodCallFailed> {
    let (listener, endpoint) = match bound {
        Some((l, ip, port)) => (Some(l), Some((ip, port))),
        None => (None, None),
    };
    // -1, not 0: 0 is a legitimate "ephemeral" request but never a legitimate
    // bound port, so it must not read back as one.
    let bound_port = endpoint.map(|(_, p)| p).unwrap_or(-1);
    let server_id = next_server_id();
    let state = std::sync::Arc::new(ServerState {
        listener: Mutex::new(listener),
        running: AtomicBool::new(false),
        handlers: Mutex::new(Vec::new()),
        bound_port: AtomicI32::new(bound_port),
        conns: OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch),
        next_conn: AtomicU64::new(0),
        vm: ctx.vm_identity(),
    });
    server_registry().lock().insert(server_id, state);
    let srv0 = try_alloc_concurrent_synthetic(ctx, class_name, 6)?;
    // `alloc_inet_socket_address_resolved` allocates, so a moving young GC can
    // relocate `srv` between the two — pin it and read the forwarded address
    // back (native stale-local family). The previous inline version wrote its
    // six slots through the pre-GC ObjectRef.
    let srv_pin = ctx.pin_native_root(srv0);
    // Fully-resolved echo (matches HotSpot: `getAddress()` returns the socket's
    // ACTUAL bound address, not the caller's original hostname string) — see
    // `alloc_inet_socket_address_resolved`.
    let sa_echo = match endpoint {
        Some((ip, port)) => {
            let ip_str = ip.to_string();
            Some(alloc_inet_socket_address_resolved(
                ctx, &ip_str, &ip_str, port,
            )?)
        }
        None => None,
    };
    let srv = ctx.read_native_pin(srv_pin, srv0);
    ctx.set_field(srv, HS_ADDRESS, Value::Object(sa_echo));
    ctx.set_field(srv, HS_STARTED, Value::Int(0));
    ctx.set_field(srv, HS_CONTEXTS, Value::Object(None));
    ctx.set_field(srv, HS_SERVER_ID, Value::Int(server_id));
    ctx.set_field(srv, HS_PORT, Value::Int(bound_port));
    ctx.set_field(srv, HS_EXECUTOR, Value::Object(None));
    ctx.unpin_native_roots(srv_pin);
    Ok(srv)
}

/// `HttpServer.create(InetSocketAddress, int)`.
///
/// `class_name` is the receiver's runtime class. Native dispatch is keyed by
/// the receiver class and `alias_class` copies a SNAPSHOT, so a factory that
/// mints `HS_IMPL_CLASS` only reaches the natives registered on `HttpServer`
/// BEFORE the alias at the end of this registrar. Callers registered after that
/// point (phase 72's `createContext(String)`, `getAttributes`, …) therefore ask
/// for the public class name instead.
pub fn re10_create_server(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    class_name: &str,
) -> MethodCallResult {
    // Handed to the listener; see `re10_open_listener`.
    let backlog = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    // `create(null, backlog)` is the documented way to obtain an UNBOUND
    // server ("If addr is null, then the bind method must be called to set
    // the address"). `obj_arg(args, 0)?` used to turn that into an NPE.
    let sa_arg = args.first().copied().unwrap_or(Value::Object(None));
    let bound = match sa_arg {
        Value::Object(Some(sa)) => Some(re10_open_listener(ctx, sa, backlog)?),
        _ => None,
    };
    let srv = re10_alloc_server(ctx, class_name, bound);
    Ok(Some(Value::Object(Some(srv?))))
}

/// `HttpServer.create()` — a server that is deliberately NOT bound yet.
///
/// Minted under `HS_IMPL_CLASS`, exactly like the two-arg factory above.
///
/// It used to mint under the PUBLIC name `com/sun/net/httpserver/HttpServer`,
/// to reach phase-72 natives registered on that key AFTER the `alias_class`
/// snapshot at the end of `register_re10_http_server` — `createContext(String)`,
/// `getAttributes()`, `getServer()`. **Those rows are not in the registry.** A
/// `--dump-native-registry` census over a boot in compatible mode reports all
/// eleven `com/sun/net/httpserver/HttpServer` rows as
/// `registered_by = net_phase_e.rs` with `overwrote = null`, and phase 72 runs
/// AFTER phase E — had it registered these keys it would own the slots. The
/// alias therefore copies the complete surface (eleven rows on each class, the
/// same eleven method keys), and nothing is lost by minting the impl class.
///
/// What WAS lost by minting the public name: `com.sun.net.httpserver.HttpServer`
/// is ABSTRACT. `create()` handed the application an instance of an abstract
/// class, so `getClass().getName()` answered a class no JDK can instantiate,
/// where HotSpot answers `sun.net.httpserver.HttpServerImpl`. That is observable
/// from ordinary code, it survived `--real-jdk`, and an `instanceof` or cast
/// against the impl type could not match. See the retired
/// `the-httpserver-family-four-defects-20260902` write-up.
pub fn re10_create_unbound_server(ctx: &mut dyn NativeContext) -> MethodCallResult {
    let srv = re10_alloc_server(ctx, HS_IMPL_CLASS, None);
    Ok(Some(Value::Object(Some(srv?))))
}

/// `HttpServer.bind(InetSocketAddress, int)` — really bind the listener.
///
/// This used to be a phase-72 no-op that stored the address in a slot, so
/// `create(); bind(addr, 0); start();` — the only sequence the no-arg factory
/// supports — could never serve a request.
pub fn re10_bind_server(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let id = ctx.get_field(this, HS_SERVER_ID).as_int().unwrap_or(-1);
    // Clone the Arc out from under the registry lock before touching the
    // per-server locks (the invariant `gc_scan_re10_handler_roots` documents).
    let state = {
        let reg = server_registry().lock();
        reg.get(&id).cloned()
    };
    let Some(state) = state else {
        return Err(ioex("HttpServer.bind: server not registered"));
    };
    if state.running.load(Ordering::SeqCst) {
        return Err(RuntimeError::IllegalStateException {
            message: "server already started".to_string(),
        }
        .into());
    }
    let already_bound = state.listener.lock().is_some();
    if already_bound {
        return Err(ioex("HttpServer.bind: server already bound"));
    }
    let sa = obj_arg(args, 1)?;
    let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
    let (listener, bound_ip, bound_port) = re10_open_listener(ctx, sa, backlog)?;
    *state.listener.lock() = Some(listener);
    state.bound_port.store(bound_port, Ordering::SeqCst);
    // Pin `this` across the address allocation (native stale-local family).
    let this_pin = ctx.pin_native_root(this);
    let ip_str = bound_ip.to_string();
    let sa_echo = alloc_inet_socket_address_resolved(ctx, &ip_str, &ip_str, bound_port);
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, HS_ADDRESS, Value::Object(Some(sa_echo?)));
    ctx.set_field(this, HS_PORT, Value::Int(bound_port));
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

