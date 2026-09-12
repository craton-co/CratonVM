// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! `java.net` / `java.nio.channels` natives: SocketChannel, Selector, HttpClient, WebSocket, datagrams, ServerSocket/Socket, com.sun.net.httpserver.
//!
//! Pure code move out of `phases_late.rs` (no logic, signature or ordering
//! changes). Every registration call site is untouched and the per-phase
//! dispatchers stay in the parent module, so the native registration SEQUENCE
//! is byte-identical to before the split.

use super::*;

// =============================================================================
// NIO Channels — SocketChannel, ServerSocketChannel, Selector, SelectionKey
// SocketChannel = 4-field synthetic (connected=0, open=1, address=2, fd_id=3)
// ServerSocketChannel = 4-field synthetic (open=0, bound=1, fd_id=2, socket=3)
// Selector = 3-field synthetic (open=0, keys_arr=1, key_count=2)
// SelectionKey = 4-field synthetic (channel=0, selector=1, interestOps=2, readyOps=3)
// =============================================================================
//
// The ServerSocketChannel line above read "3-field synthetic (open=0, bound=1,
// fd_id=2)" until 2026-08-12 and had been wrong since the slot-3 socket cache
// was added: `open` allocates 4 and every `ssc` body below indexes 0..=3.
// Corrected rather than deleted, because the count is what `SSC_P58_SLOT_MAP`
// publishes (W7-88-net-channels-dead-registration.md).

/// What this registrar believes the slots of `java/nio/channels/ServerSocketChannel`
/// are — published so `read_alias::verify_declared_slot_maps` sweeps it against
/// whichever class is actually loaded.
///
/// Every entry is wrong on the real JDK 25 class, and that disagreement IS the
/// census row, so it is stated as the belief and not corrected to `javap`.
/// The real transitive layout (Eclipse Adoptium 25.0.3.9, superclass first,
/// declaration order, `static` excluded) is
///
/// ```text
///   0 closeLock  1 closed  2 interruptor  3 interruptedTarget   (AbstractInterruptibleChannel)
///   4 provider   5 keys    6 keyCount     7 keyLock  8 regLock  9 nonBlocking
///                                                     (AbstractSelectableChannel)
/// ```
///
/// so slot 1 is the `closed` flag a real `isOpen()` reads (this registrar puts
/// the BOUND flag there), slot 2 is the `sun.nio.ch.Interruptible` (it puts an
/// `int` fd there), and slot 3 is `interruptedTarget`, which
/// `AbstractInterruptibleChannel.end(boolean)` reads on every interruptible
/// operation. `keys` at 5 is the field W7-72-ssc-socket-and-filechannel.md §1
/// repaired on the winning `native-io` side.
///
/// Nothing dispatches to the bodies holding this belief — `register_p58_nio_channels`
/// is reachable only from `register_synthetic_overrides`, and all nine of its
/// `ssc` triples are re-registered later by `native-io`'s
/// `register_socket_channel_real` in every arm. Publishing the map is therefore
/// a measurement, not a repair: if the registrar ever escapes that gate, the
/// sweep names the fields instead of a suite finding them.
pub static SSC_P58_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap =
    cratonvm_native_api::read_alias::SlotMap {
        class: "java/nio/channels/ServerSocketChannel",
        slots: &[
            (0, "open"),
            (1, "bound"),
            (2, "fd"),
            // Slot 3 lost its only producer when `socket()` was deleted
            // (W7-88); `open` still nulls it and two `bind` arms still read it,
            // so the belief is still held and still published.
            (3, "socket"),
        ],
        origin: "native-builtins/src/phases_late/net_channels.rs register_p58_nio_channels",
    };

pub(crate) fn register_p58_nio_channels(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Unconditional, matching `register_io_natives`' publication of
    // `BB_SLOT_MAP` and `register_phase52_time_enums`' of `MONTH_SLOT_MAP`:
    // gating it on `CRATONVM_DBG_LAYOUT_ALIAS` would leave a run that enables
    // the flag later with nothing to sweep, which is a detector reporting clean
    // because it cannot see.
    cratonvm_native_api::read_alias::declare_slot_map(&SSC_P58_SLOT_MAP);
    let sc = "java/nio/channels/SocketChannel";
    r.register(
        sc,
        "open",
        "()Ljava/nio/channels/SocketChannel;",
        |ctx, _args| {
            let sc = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 4)?;
            ctx.set_field(sc, 0, Value::Int(0));
            ctx.set_field(sc, 1, Value::Int(1));
            ctx.set_field(sc, 2, Value::Object(None));
            ctx.set_field(sc, 3, Value::Int(-1));
            Ok(Some(Value::Object(Some(sc))))
        },
    );
    r.register(
        sc,
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
        |ctx, args| {
            let addr_obj = args.first().copied().unwrap_or(Value::Object(None));
            let addr_str = p98_extract_socket_addr(ctx, addr_obj);
            let mut sc_obj =
                try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 4)?;
            match crate::capability_gate::open_tcp_connect_gated(&*ctx, &addr_str) {
                Ok(fd) => {
                    ctx.set_field(sc_obj, 0, Value::Int(1));
                    ctx.set_field(sc_obj, 1, Value::Int(1));
                    // Pin across the create_string below — a moving young GC
                    // there would relocate the fresh channel (native
                    // stale-local family).
                    let sc_pin = ctx.pin_native_root(sc_obj);
                    let s = ctx.create_string(&addr_str);
                    sc_obj = ctx.read_native_pin(sc_pin, sc_obj);
                    ctx.set_field(sc_obj, 2, Value::Object(Some(s)));
                    ctx.set_field(sc_obj, 3, Value::Int(fd as i32));
                    ctx.unpin_native_roots(sc_pin);
                }
                Err(_) => {
                    ctx.set_field(sc_obj, 0, Value::Int(0));
                    ctx.set_field(sc_obj, 1, Value::Int(1));
                    ctx.set_field(sc_obj, 2, Value::Object(None));
                    ctx.set_field(sc_obj, 3, Value::Int(-1));
                }
            }
            Ok(Some(Value::Object(Some(sc_obj))))
        },
    );
    r.register(sc, "connect", "(Ljava/net/SocketAddress;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.get_field(this, 1).as_int().unwrap_or(0) == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "Channel is closed".into(),
            }
            .into());
        }
        let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
        let addr_str = p98_extract_socket_addr(ctx, addr_obj);
        match crate::capability_gate::open_tcp_connect_gated(&*ctx, &addr_str) {
            Ok(fd) => {
                ctx.set_field(this, 0, Value::Int(1));
                // Pin across the create_string below — a moving young GC there
                // would relocate `this` (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let s = ctx.create_string(&addr_str);
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field(this, 2, Value::Object(Some(s)));
                ctx.set_field(this, 3, Value::Int(fd as i32));
                ctx.unpin_native_roots(this_pin);
                Ok(Some(Value::Int(1)))
            }
            Err(_) => Ok(Some(Value::Int(0))),
        }
    });
    r.register(sc, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(sc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(sc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd >= 0 {
            let _ = ctx.fd_table().close(fd as u32);
        }
        ctx.set_field(this, 0, Value::Int(0));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 3, Value::Int(-1));
        Ok(None)
    });
    // read(ByteBuffer) — ByteBuffer: field 0=byte[], 1=position, 2=limit, 3=capacity
    r.register(sc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let bb = obj_arg(args, 1)?;
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let limit = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = if limit > pos { limit - pos } else { 0 };
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; remaining];
        // STW-TAKEOVER-FIX (2026-07-14): plain blocking-mode
        // `SocketChannel.read` — the real socket path used by embedded
        // Tomcat/Jetty NIO connectors for accepted client connections
        // (unlike `AsynchronousSocketChannel`, this is the hot path for
        // ordinary server sockets). Same missing-GC-barrier-cooperation gap
        // as the `AsynchronousSocketChannel.read`/`write` fix above: a
        // genuinely blocking OS recv() with no bound on wait time, with no
        // `begin_blocking_region`/`end_blocking_region` around it, so a
        // concurrent STW pause waits forever for this thread to reach a
        // safepoint it can never reach while parked in recv(). `bb` is an
        // ObjectRef used again after the call returns (both `get_field`/
        // `set_field` on it), so it goes through
        // `end_blocking_region_refs` to pick up any relocation from a GC
        // that ran while blocked.
        let mut blocked_refs = [Value::Object(Some(bb))];
        ctx.begin_blocking_region();
        let read_result = ctx.fd_table().tcp_read(fd as u32, &mut buf);
        ctx.end_blocking_region_refs(&mut blocked_refs);
        let bb = match blocked_refs[0] {
            Value::Object(Some(o)) => o,
            _ => bb,
        };
        match read_result {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                    for i in 0..n {
                        ctx.set_array_element(arr, pos + i, Value::Int(buf[i] as i8 as i32));
                    }
                }
                ctx.set_field(bb, 1, Value::Int((pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Some(Value::Int(0))),
            Err(_) => Ok(Some(Value::Int(-1))),
        }
    });
    // write(ByteBuffer) — write from ByteBuffer to TCP
    r.register(sc, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd < 0 {
            return Ok(Some(Value::Int(0)));
        }
        let bb = obj_arg(args, 1)?;
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let limit = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = if limit > pos { limit - pos } else { 0 };
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut data = vec![0u8; remaining];
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for i in 0..remaining {
                if let Value::Int(b) = ctx.get_array_element(arr, pos + i) {
                    data[i] = b as u8;
                }
            }
        }
        // STW-TAKEOVER-FIX (2026-07-14): see `SocketChannel.read` above —
        // same genuinely-blocking-socket-call gap. `arr` was already fully
        // consumed above (copied into the Rust-owned `data` buffer before
        // this call), so only `bb` needs to survive the blocking window.
        let mut blocked_refs = [Value::Object(Some(bb))];
        ctx.begin_blocking_region();
        let write_result = ctx.fd_table().tcp_write(fd as u32, &data);
        ctx.end_blocking_region_refs(&mut blocked_refs);
        let bb = match blocked_refs[0] {
            Value::Object(Some(o)) => o,
            _ => bb,
        };
        match write_result {
            Ok(n) => {
                ctx.set_field(bb, 1, Value::Int((pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Some(Value::Int(0))),
            Err(_) => Ok(Some(Value::Int(-1))),
        }
    });
    r.register(
        sc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            let fd = ctx.get_field(this, 3).as_int().unwrap_or(-1);
            if fd >= 0 {
                let _ = ctx.fd_table().tcp_set_nonblocking(fd as u32, blocking == 0);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(sc, "finishConnect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        sc,
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 2)))
        },
    );

    // ServerSocketChannel = 4-field synthetic (open=0, bound=1, fd_id=2, cached_socket=3)
    let ssc = "java/nio/channels/ServerSocketChannel";
    r.register(
        ssc,
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
        |ctx, _args| {
            let ssc =
                try_alloc_concurrent_synthetic(ctx, "java/nio/channels/ServerSocketChannel", 4)?;
            ctx.set_field(ssc, 0, Value::Int(1));
            ctx.set_field(ssc, 1, Value::Int(0));
            ctx.set_field(ssc, 2, Value::Int(-1));
            ctx.set_field(ssc, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(ssc))))
        },
    );
    r.register(
        ssc,
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
            let addr_str = p98_extract_socket_addr(ctx, addr_obj);
            if crate::nbflags().dbg_nio_bind {
                eprintln!("[NIO_BIND] ssc.bind 1-arg addr='{}'", addr_str);
            }
            // GAP I6: server-socket bind was ungated — binding is an authority
            // in its own right (a listener on 0.0.0.0 exposes the host), not a
            // weaker form of connecting.
            match crate::capability_gate::open_tcp_listener_gated(&*ctx, &addr_str) {
                Ok(fd) => {
                    ctx.set_field(this, 1, Value::Int(1));
                    ctx.set_field(this, 2, Value::Int(fd as i32));
                    if crate::nbflags().dbg_nio_bind {
                        eprintln!("[NIO_BIND] ssc.bind ok fd={fd}");
                    }
                    // If a wrapper ServerSocket has been cached, mirror the actual local port
                    // so getLocalPort/getLocalSocketAddress return the OS-chosen port.
                    if let Value::Object(Some(s)) = ctx.get_field(this, 3) {
                        if let Ok(local) = ctx.fd_table().tcp_local_addr(fd) {
                            let port = local
                                .rsplit(':')
                                .next()
                                .and_then(|p| p.parse::<i32>().ok())
                                .unwrap_or(0);
                            ctx.set_field(s, 0, Value::Int(port)); // SS_PORT
                        }
                    }
                    Ok(Some(Value::Object(Some(this))))
                }
                // A capability refusal is a `SecurityException`, not the
                // `IOException` a failed bind raises.
                Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
                    Err(denied.into())
                }
                Err(e) => {
                    if crate::nbflags().dbg_nio_bind {
                        eprintln!("[NIO_BIND] ssc.bind FAILED addr='{}' err={}", addr_str, e);
                    }
                    Err(RuntimeError::IOException {
                        message: format!("ServerSocketChannel.bind {}: {}", addr_str, e),
                    }
                    .into())
                }
            }
        },
    );
    // 2-arg variant: `bind(SocketAddress, int backlog)`. NioEndpoint calls
    // this one with `getAcceptCount()` as backlog. Without an explicit
    // override the JDK default routes through the 1-arg version, but the
    // real ServerSocketChannelImpl has a concrete 2-arg method that bypasses
    // our 1-arg native — so register the same body for both signatures.
    r.register(
        ssc,
        "bind",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
            let addr_str = p98_extract_socket_addr(ctx, addr_obj);
            if crate::nbflags().dbg_nio_bind {
                let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
                eprintln!(
                    "[NIO_BIND] ssc.bind 2-arg addr='{}' backlog={}",
                    addr_str, backlog
                );
            }
            // GAP I6 — see the 1-arg `bind` above.
            match crate::capability_gate::open_tcp_listener_gated(&*ctx, &addr_str) {
                Ok(fd) => {
                    ctx.set_field(this, 1, Value::Int(1));
                    ctx.set_field(this, 2, Value::Int(fd as i32));
                    if crate::nbflags().dbg_nio_bind {
                        eprintln!("[NIO_BIND] ssc.bind 2-arg ok fd={fd}");
                    }
                    if let Value::Object(Some(s)) = ctx.get_field(this, 3) {
                        if let Ok(local) = ctx.fd_table().tcp_local_addr(fd) {
                            let port = local
                                .rsplit(':')
                                .next()
                                .and_then(|p| p.parse::<i32>().ok())
                                .unwrap_or(0);
                            ctx.set_field(s, 0, Value::Int(port));
                        }
                    }
                    Ok(Some(Value::Object(Some(this))))
                }
                // A capability refusal is a `SecurityException`.
                Err(cratonvm_native_api::fd_table::FdCapabilityError::Denied(denied)) => {
                    Err(denied.into())
                }
                Err(e) => {
                    if crate::nbflags().dbg_nio_bind {
                        eprintln!(
                            "[NIO_BIND] ssc.bind 2-arg FAILED addr='{}' err={}",
                            addr_str, e
                        );
                    }
                    Err(RuntimeError::IOException {
                        message: format!("ServerSocketChannel.bind {}: {}", addr_str, e),
                    }
                    .into())
                }
            }
        },
    );
    // `ServerSocketChannel.socket()Ljava/net/ServerSocket;` USED TO BE REGISTERED
    // HERE, and was DELETED on 2026-08-12 — W7-88-net-channels-dead-registration.md,
    // residual 7 of W7-72-ssc-socket-and-filechannel.md. It is not a repair:
    // deleting it cannot change behaviour, and that is exactly why it went.
    //
    // Why it could not run. `--dump-native-registry` on the prebuilt dev binary
    // shows `native-io/src/socket_channel.rs:4776` owning this triple with
    // `overwrote = null` in all four runnable configurations (default build,
    // `--real-jdk` and `--jdk-only`, each also with `CRATONVM_REAL=-net-sockets`)
    // — `null`, not "overwrote net_channels", because THIS registrar never ran
    // at all: every net_channels row in those censuses is at line >= 1252, i.e.
    // `register_p58_nio_channels` contributed nothing. Its only path in is
    // `register_phase58_natives` <- `register_synthetic_overrides`, which is
    // `#[cfg(feature = "synthetic-jdk")]`, and the default `cratonvm-cli` build
    // reports `jdk.mode.synthetic_compiled_in = false`. In the feature build it
    // does register and still loses: `vm_init.rs` calls `register_builtins` and
    // then `register_io_natives` on the NEXT line, and `register()` is
    // last-write-wins.
    //
    // Why deleting beat correcting. The body wrote SEVEN real JDK fields.
    // `try_alloc_concurrent_synthetic("java/net/ServerSocket", 5)` clamps up to
    // the real class's six declared slots, so its map (SS_PORT=0, SS_BACKLOG=1,
    // SS_CLOSED=2, SS_LISTENER_ID=3, channel_ref=4) landed on
    // `impl`/`created`/`bound`/`closed`/`socketLock` — `closed := -1` alone makes
    // a `java.net.ServerSocket` report itself CLOSED to its own bytecode — and
    // `set_field(this, 3, ss)` put that ServerSocket into
    // `AbstractInterruptibleChannel.interruptedTarget`, which `end(boolean)`
    // reads on EVERY interruptible operation and, when non-null, follows with
    // `interruptor.postInterrupt()`. That is a strictly worse version of the
    // `keys`-slot defect W7-72 §1 repaired. Correcting the map for the real
    // layout would have broken the fabricated layout this body was written for
    // (the W7-66 shape); correcting it for the fabricated layout is an inert
    // fix, because the registration loses in that arm too.
    //
    // NOT fixed here: the other eight `ssc` triples in this registrar keep the
    // same 4-slot map over the same real class. Each also has a later
    // `native-io` twin (measured — W7-88 §4), so they are dead the same way.
    // `SSC_P58_SLOT_MAP` publishes the belief so the read-side sweep can see
    // them, instead of a comment asserting they are fine.
    // ServerSocketChannel.getLocalAddress() — return InetSocketAddress with local port
    r.register(
        ssc,
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd = ctx.get_field(this, 2).as_int().unwrap_or(-1);
            if fd < 0 {
                return Ok(Some(Value::Object(None)));
            }
            let local = match ctx.fd_table().tcp_local_addr(fd as u32) {
                Ok(s) => s,
                Err(_) => return Ok(Some(Value::Object(None))),
            };
            let (host, port_s) = match local.rsplit_once(':') {
                Some((h, p)) => (h, p),
                None => ("0.0.0.0", "0"),
            };
            let port = port_s.parse::<i32>().unwrap_or(0);
            let isa = try_alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh address (native stale-local family).
            let isa_pin = ctx.pin_native_root(isa);
            let host_str = ctx.create_string(host);
            let isa = ctx.read_native_pin(isa_pin, isa);
            ctx.set_field(isa, 0, Value::Object(Some(host_str)));
            ctx.set_field(isa, 1, Value::Int(port));
            ctx.unpin_native_roots(isa_pin);
            Ok(Some(Value::Object(Some(isa))))
        },
    );
    r.register(
        ssc,
        "accept",
        "()Ljava/nio/channels/SocketChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd = ctx.get_field(this, 2).as_int().unwrap_or(-1);
            if fd < 0 {
                return Ok(Some(Value::Object(None)));
            }
            // STW-TAKEOVER-FIX (2026-07-14): `ServerSocketChannel.accept()`
            // blocks indefinitely (in blocking mode) waiting for an
            // incoming connection — the same missing-GC-barrier-cooperation
            // gap as the `SocketChannel.read`/`write` fixes above, on the
            // acceptor-thread side this time. No ObjectRef needs to survive
            // the call (`this` is not reused afterward; a fresh object is
            // allocated post-accept), so a plain begin/end pair suffices.
            ctx.begin_blocking_region();
            let accept_result = ctx.fd_table().tcp_accept(fd as u32);
            ctx.end_blocking_region();
            match accept_result {
                Ok((stream_fd, addr)) => {
                    let sc =
                        try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 4)?;
                    // Pin across the create_string below — a moving young GC
                    // there would relocate the fresh channel (native
                    // stale-local family).
                    let sc_pin = ctx.pin_native_root(sc);
                    ctx.set_field(sc, 0, Value::Int(1));
                    ctx.set_field(sc, 1, Value::Int(1));
                    let s = ctx.create_string(&addr);
                    let sc = ctx.read_native_pin(sc_pin, sc);
                    ctx.set_field(sc, 2, Value::Object(Some(s)));
                    ctx.set_field(sc, 3, Value::Int(stream_fd as i32));
                    ctx.unpin_native_roots(sc_pin);
                    Ok(Some(Value::Object(Some(sc))))
                }
                Err(_) => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(ssc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(ssc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd = ctx.get_field(this, 2).as_int().unwrap_or(-1);
        if fd >= 0 {
            let _ = ctx.fd_table().close(fd as u32);
        }
        ctx.set_field(this, 0, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(-1));
        Ok(None)
    });
    r.register(
        ssc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            let fd = ctx.get_field(this, 2).as_int().unwrap_or(-1);
            if fd >= 0 {
                let _ = ctx.fd_table().tcp_set_nonblocking(fd as u32, blocking == 0);
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    // Selector = 4-field synthetic (open=0, keys_arr=1, key_count=2, wakeup_flag=3)
    let sel = "java/nio/channels/Selector";
    r.register(
        sel,
        "open",
        "()Ljava/nio/channels/Selector;",
        |ctx, _args| {
            if crate::nbflags().dbg_sel {
                eprintln!("[SEL/p98] Selector.open()");
            }
            let sel = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/Selector", 4)?;
            // Pin across the keys-array alloc below — a moving young GC there
            // would relocate the fresh Selector (native stale-local family).
            let sel_pin = ctx.pin_native_root(sel);
            ctx.set_field(sel, 0, Value::Int(1));
            let keys = ctx.new_array(cratonvm_types::ArrayElementType::Reference, 64);
            let sel = ctx.read_native_pin(sel_pin, sel);
            ctx.set_field(sel, 1, Value::Object(Some(keys)));
            ctx.set_field(sel, 2, Value::Int(0));
            ctx.set_field(sel, 3, Value::Int(0)); // wakeup_flag
            ctx.unpin_native_roots(sel_pin);
            Ok(Some(Value::Object(Some(sel))))
        },
    );
    // select() — blocks until at least one channel is ready or wakeup is called (max 30s)
    r.register(sel, "select", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        p98_blocking_select(ctx, this, 30_000)
    });
    // select(long timeout) — blocks up to timeout_ms (0 means infinite → cap at 30s)
    r.register(sel, "select", "(J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout_ms = match args.get(1) {
            Some(Value::Long(t)) => *t,
            _ => 0,
        };
        let effective = if timeout_ms <= 0 {
            30_000i64
        } else {
            timeout_ms
        };
        p98_blocking_select(ctx, this, effective)
    });
    // selectNow() — single non-blocking poll
    r.register(sel, "selectNow", "()I", p98_selector_select);
    r.register(sel, "selectedKeys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_count = ctx.get_field(this, 2).as_int().unwrap_or(0) as usize;
        let mut ready = Vec::new();
        if let Value::Object(Some(ka)) = ctx.get_field(this, 1) {
            for i in 0..key_count {
                if let Value::Object(Some(k)) = ctx.get_array_element(ka, i) {
                    if ctx.get_field(k, 3).as_int().unwrap_or(0) != 0 {
                        ready.push(k);
                    }
                }
            }
        }
        // Pin across the set/array allocs inside the builder — a moving young
        // GC there would relocate the collected keys (native stale-local
        // family). `build_real_hash_set` re-pins them itself; these pins cover
        // the gap between collecting them above and handing them over.
        let ready_pins: Vec<usize> = ready.iter().map(|k| ctx.pin_native_root(*k)).collect();
        let ready: Vec<ObjectRef> = ready
            .iter()
            .enumerate()
            .map(|(i, k)| ctx.read_native_pin(ready_pins[i], *k))
            .collect();
        // Through `HashSet.<init>` and `add`, not by writing absolute slots 0
        // and 1. Those two are the MAP layout on a class whose one real field
        // is `map`, so `selectedKeys().size()` / `iterator()` / `remove()` --
        // the three methods a selector loop actually calls, and which this VM
        // does NOT register for `HashSet` past `add` -- all read through a
        // `map` holding an `Object[]` and answered for an empty set.
        let set = crate::build_real_hash_set(ctx, &ready)?;
        if let Some(base) = ready_pins.first().copied() {
            ctx.unpin_native_roots(base);
        }
        Ok(Some(Value::Object(Some(set))))
    });
    r.register(sel, "keys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let kc = ctx.get_field(this, 2).as_int().unwrap_or(0) as usize;
        // Pin across the set/array allocs below — a moving young GC there
        // would relocate `this` (native stale-local family).
        let this_pin = ctx.pin_native_root(this);
        let mut keys: Vec<ObjectRef> = Vec::with_capacity(kc);
        if let Value::Object(Some(ka)) = ctx.get_field(this, 1) {
            for i in 0..kc {
                if let Value::Object(Some(k)) = ctx.get_array_element(ka, i) {
                    keys.push(k);
                }
            }
        }
        // Same conversion, same reason, as `selectedKeys` above: the real
        // `HashSet.<init>` and `add`, never absolute slots 0 and 1.
        let set = crate::build_real_hash_set(ctx, &keys)?;
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Object(Some(set))))
    });
    r.register(
        sel,
        "wakeup",
        "()Ljava/nio/channels/Selector;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 3, Value::Int(1)); // set wakeup flag
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(sel, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(sel, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0));
        Ok(None)
    });

    // SelectionKey = 4-field synthetic (channel=0, selector=1, interestOps=2, readyOps=3)
    //
    // W7-9 — that "4-field synthetic" is a real JDK class, and this layout is
    // laid over its declared one. `javap -p java.nio.channels.SelectionKey` on
    // JDK 25 gives exactly one instance field,
    // `private volatile java.lang.Object attachment`, at slot 0 — a REFERENCE.
    // `try_alloc_concurrent_synthetic` takes `num_fields.max(real)` = 4 and
    // reports an alias only when `num_fields < real`, so requesting 4 against
    // a declared 1 is silent. Slot 0 happens to hold a reference in both
    // readings (`channel` here, `attachment` there) which is why nothing has
    // caught fire, but the two disagree about WHICH reference, and
    // `nio_selector.rs`'s `sk_attach`/`sk_attachment` read the real one.
    // Same species as W4-4/W6-3 in a third file; the census is in
    // docs/known-issues/jdk-only/W7-9-minted-interface-abstract-methods.md §5.
    // Not repaired here: this registrar is synthetic-only (reached from
    // `register_synthetic_overrides`), the live SelectionKey surface is
    // `native-io/src/nio_selector.rs::register_nio_selector_real`, and a
    // one-sided renumbering would only move the disagreement.
    let sk = "java/nio/channels/SelectionKey";

    // The four public interest-op constants, registered with the FIELD
    // descriptor "I" rather than a method one.
    //
    // THE CLAIM THIS COMMENT USED TO MAKE IS FALSE, and the note ~40 lines
    // below says so while these four rows are still here — the file
    // contradicted itself. `key.interestOps(SelectionKey.OP_READ)` STILL has no
    // way to reach the value 1 through these rows: javac emits `GETSTATIC` for
    // a `static final int`, and no `GETSTATIC` path in this VM consults the
    // native registry (the three paths are enumerated at the
    // `HttpClient$Version` registration below). They are dead in both run
    // modes, reachable only by an explicit `call_native` — which is exactly
    // what `vm/src/vm/tests.rs`'s `selection_key_constants_p58` does, so the
    // test passes while nothing on the bytecode side can select them.
    //
    // Left in place rather than deleted only because deleting them turns that
    // test red in a file this lane does not own; the removal, and the
    // `<clinit>` replacement that would make the constants actually readable,
    // are nominated in
    // docs/known-issues/jdk-only/E21-1-getstatic-has-no-native-path.md.
    // The values are fixed by the JDK spec.
    r.register(sk, "OP_READ", "I", |_ctx, _args| Ok(Some(Value::Int(1))));
    r.register(sk, "OP_WRITE", "I", |_ctx, _args| Ok(Some(Value::Int(4))));
    r.register(sk, "OP_CONNECT", "I", |_ctx, _args| Ok(Some(Value::Int(8))));
    r.register(sk, "OP_ACCEPT", "I", |_ctx, _args| Ok(Some(Value::Int(16))));
    r.register(
        sk,
        "channel",
        "()Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        sk,
        "selector",
        "()Ljava/nio/channels/Selector;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );
    r.register(sk, "interestOps", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });
    r.register(sk, "readyOps", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 3)))
    });
    r.register(sk, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // interestOps = -1 indicates a cancelled key
        let ops = ctx.get_field(this, 2).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if ops >= 0 { 1 } else { 0 })))
    });
    r.register(sk, "cancel", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Mark as cancelled by setting interestOps to -1
        ctx.set_field(this, 2, Value::Int(-1));
        ctx.set_field(this, 3, Value::Int(0)); // clear ready ops
        Ok(None)
    });

    // NOT REMOVED — this note said "REMOVED (stub-removal wave 2)" and was
    // wrong: the four `SelectionKey.OP_READ/OP_WRITE/OP_CONNECT/OP_ACCEPT`
    // rows it describes are still registered ~40 lines above, where a second
    // comment used to claim they made the constants reachable. Corrected in
    // place on 2026-08-13 rather than deleted, because the DIAGNOSIS below is
    // right and is the one this file's `HttpClient$Version` fix rests on:
    //
    // The registry is keyed on (class, method, descriptor) and every lookup
    // comes from an invoke instruction, whose descriptor always starts with
    // '('; javac emits `getstatic` for these `static final int` constants and
    // there is no getstatic-to-native path. The four entries can therefore
    // never be selected in either run mode — dead registrations, not stubs.
    // (`native-io/src/lib.rs` separately registers them with a well-formed
    // "()I" descriptor; that one is at least reachable by an explicit call.)

    let sac = "java/nio/channels/SelectableChannel";
    r.register(
        sac,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        |ctx, args| {
            let channel = obj_arg(args, 0)?;
            let selector = obj_arg(args, 1)?;
            let ops = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            // Pin across the key alloc below — a moving young GC there would
            // relocate them (native stale-local family).
            let channel_pin = ctx.pin_native_root(channel);
            let selector_pin = ctx.pin_native_root(selector);
            let key = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SelectionKey", 4)?;
            let channel = ctx.read_native_pin(channel_pin, channel);
            let selector = ctx.read_native_pin(selector_pin, selector);
            ctx.unpin_native_roots(channel_pin);
            ctx.set_field(key, 0, Value::Object(Some(channel)));
            ctx.set_field(key, 1, Value::Object(Some(selector)));
            ctx.set_field(key, 2, Value::Int(ops));
            ctx.set_field(key, 3, Value::Int(0));
            let kc = ctx.get_field(selector, 2).as_int().unwrap_or(0) as usize;
            if let Value::Object(Some(ka)) = ctx.get_field(selector, 1) {
                if kc < ctx.array_length(ka) {
                    ctx.set_array_element(ka, kc, Value::Object(Some(key)));
                    ctx.set_field(selector, 2, Value::Int((kc + 1) as i32));
                }
            }
            Ok(Some(Value::Object(Some(key))))
        },
    );
    r.register(
        sac,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    let socket_adaptor = "sun/nio/ch/SocketAdaptor";
    r.register(
        socket_adaptor,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| p72_socket_adaptor_address(ctx, args, false),
    );
    r.register(
        socket_adaptor,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| p72_socket_adaptor_address(ctx, args, true),
    );
    r.set_category(__prev_cat);
}

/// Extract host:port from a SocketAddress synthetic object.
///
/// Handles three layouts:
/// - **CratonVM synthetic 2-field** (legacy probes): field 0 = host string,
///   field 1 = port int.
/// - **Real-JDK 25 `InetSocketAddress`**: one `holder` field of type
///   `InetSocketAddressHolder` with `{hostname, addr, port}`. We follow the
///   `holder` chain and read `port` + `hostname` by name.
/// - **Unknown / unresolved**: fall back to `"0.0.0.0:0"` so the listener
///   binds to the wildcard ephemeral port (matches HotSpot behaviour for a
///   `null` address). Returning `"127.0.0.1:0"` here was wrong — Tomcat's
///   server connectors expect to bind on the all-interfaces wildcard, and a
///   loopback-only listener fails the regression probe later when something
///   tries to connect from outside.
pub(crate) fn p98_extract_socket_addr(ctx: &mut dyn NativeContext, addr: Value) -> String {
    let a = match addr {
        Value::Object(Some(a)) => a,
        _ => return "0.0.0.0:0".to_string(),
    };

    // Real-JDK path first: probe for `holder` field by name. If present, the
    // host + port live one level deeper via `holder.hostname` /
    // `holder.addr.hostName` / `holder.port`. This is the layout JDK 25 ships
    // and Tomcat / Netty / Jetty all hand us.
    match ctx.get_field_by_name(a, "holder") {
        Value::Object(Some(h)) => {
            let port = match ctx.get_field_by_name(h, "port") {
                Value::Int(p) => p,
                _ => 0,
            };
            // Prefer holder.hostname when set; fall back to holder.addr's
            // host string. If neither resolves, use the wildcard so the
            // bind succeeds on all interfaces.
            let host = match ctx.get_field_by_name(h, "hostname") {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            let host = if !host.is_empty() {
                host
            } else if let Value::Object(Some(ia)) = ctx.get_field_by_name(h, "addr") {
                // InetAddress.holder.hostName / InetAddress.holder.address fallback.
                match ctx.get_field_by_name(ia, "holder") {
                    Value::Object(Some(iah)) => match ctx.get_field_by_name(iah, "hostName") {
                        Value::Object(Some(s)) => {
                            ctx.read_string(s).unwrap_or_else(|| "0.0.0.0".into())
                        }
                        _ => "0.0.0.0".into(),
                    },
                    _ => "0.0.0.0".into(),
                }
            } else {
                "0.0.0.0".into()
            };
            return format!("{}:{}", host, port);
        }
        _ => {}
    }

    // Synthetic 2-field fallback (the legacy probe layout).
    let host = match ctx.get_field(a, 0) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_else(|| "0.0.0.0".to_string()),
        _ => "0.0.0.0".to_string(),
    };
    let port = ctx.get_field(a, 1).as_int().unwrap_or(0);
    format!("{}:{}", host, port)
}

pub(crate) fn p98_selector_select(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    p98_do_select(ctx, this)
}

/// Blocking select: loops polling with 1ms sleeps until ready_count > 0 or wakeup flag is set.
pub(crate) fn p98_blocking_select(
    ctx: &mut dyn NativeContext,
    mut selector: ObjectRef,
    timeout_ms: i64,
) -> MethodCallResult {
    // Clear wakeup flag at start
    ctx.set_field(selector, 3, Value::Int(0));

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
    loop {
        let result = p98_do_select(ctx, selector)?;
        let count = result.as_ref().and_then(|v| v.as_int()).unwrap_or(0);
        if count > 0 {
            return Ok(result);
        }
        // Check wakeup flag
        if ctx.get_field(selector, 3).as_int().unwrap_or(0) != 0 {
            ctx.set_field(selector, 3, Value::Int(0)); // consume wakeup
            return Ok(Some(Value::Int(0)));
        }
        // Check timeout
        if std::time::Instant::now() >= deadline {
            return Ok(Some(Value::Int(0)));
        }
        // GC-safety: this is a poll loop that can run for the whole caller
        // timeout (an event loop's `select(t)`). Without a blocking region the
        // thread is neither at a safepoint nor GC-cooperative while it sleeps,
        // so a stop-the-world collector waits out the full timeout behind it.
        // `selector` is re-read through `end_blocking_region_refs` because a
        // collection completing inside the sleep can relocate it.
        ctx.begin_timed_blocking_region();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let mut refs = [Value::Object(Some(selector))];
        ctx.end_blocking_region_refs(&mut refs);
        if let Value::Object(Some(moved)) = refs[0] {
            selector = moved;
        }
    }
}

/// Single poll pass across all registered keys. Clears readyOps before polling.
pub(crate) fn p98_do_select(ctx: &mut dyn NativeContext, selector: ObjectRef) -> MethodCallResult {
    let key_count = ctx.get_field(selector, 2).as_int().unwrap_or(0) as usize;
    let mut ready_count = 0i32;
    if let Value::Object(Some(ka)) = ctx.get_field(selector, 1) {
        for i in 0..key_count {
            if let Value::Object(Some(key)) = ctx.get_array_element(ka, i) {
                // Clear readyOps at start of each poll cycle
                ctx.set_field(key, 3, Value::Int(0));
                let interest = ctx.get_field(key, 2).as_int().unwrap_or(0);
                let fd = if let Value::Object(Some(ch)) = ctx.get_field(key, 0) {
                    let fd3 = ctx.get_field(ch, 3).as_int().unwrap_or(-1);
                    if fd3 >= 0 {
                        fd3
                    } else {
                        ctx.get_field(ch, 2).as_int().unwrap_or(-1)
                    }
                } else {
                    -1
                };
                if fd >= 0 {
                    let (readable, writable) = ctx.fd_table().poll_ready(fd as u32);
                    let mut ready = 0;
                    if readable && (interest & 1 != 0) {
                        ready |= 1;
                    } // OP_READ
                    if writable && (interest & 4 != 0) {
                        ready |= 4;
                    } // OP_WRITE
                    if writable && (interest & 8 != 0) {
                        ready |= 8;
                    } // OP_CONNECT
                    if readable && (interest & 16 != 0) {
                        ready |= 16;
                    } // OP_ACCEPT
                    ctx.set_field(key, 3, Value::Int(ready));
                    if ready != 0 {
                        ready_count += 1;
                    }
                }
            }
        }
    }
    Ok(Some(Value::Int(ready_count)))
}

// =============================================================================
// java.net.http — HttpClient, HttpRequest, HttpResponse
// HttpClient = 1-field synthetic (version=0 Int: 1=HTTP/1.1, 2=HTTP/2)
// HttpRequest = 3-field synthetic (uri=0, method=1, headers=2)
// HttpResponse = 3-field synthetic (statusCode=0 Int, body=1, headers=2)
// =============================================================================

pub(crate) const HTTP_CLIENT_VERSION: &str = "java/net/http/HttpClient$Version";
pub(crate) const HTTP_CLIENT_REDIRECT: &str = "java/net/http/HttpClient$Redirect";

/// `java.net.http.HttpClient$Version`'s constants **in declaration order**.
///
/// The index into this slice IS the ordinal, so the order is load-bearing:
/// `Enum.compareTo` is `this.ordinal - other.ordinal`, `EnumMap`/`EnumSet` key
/// on it, and `values()` must hand it back in the same order. Measured on the
/// oracle, not assumed — `javap -p java.net.http.HttpClient$Version` on this
/// host (Microsoft build 25.0.3+9-LTS) declares `HTTP_1_1` then `HTTP_2`, and a
/// run of `values()` reports `ordinal()` 0 and 1 respectively.
pub(crate) const HTTP_VERSION_CONSTANTS: &[&str] = &["HTTP_1_1", "HTTP_2"];

/// `java.net.http.HttpClient$Redirect`'s constants in declaration order:
/// `NEVER`(0), `ALWAYS`(1), `NORMAL`(2). Measured the same way. Note it is
/// neither alphabetical nor least-to-most permissive — a plausible-looking
/// reordering here silently changes `compareTo` and `EnumSet` iteration.
pub(crate) const HTTP_REDIRECT_CONSTANTS: &[&str] = &["NEVER", "ALWAYS", "NORMAL"];

/// Mint and publish an enum's constants the way its real `<clinit>` would.
///
/// Registered as `("<class>", "<clinit>", "()V")`, which is the ONLY shape a
/// `GETSTATIC` can reach through (see the long note at the registration site).
/// Three obligations, each of which is silent when dropped:
///
/// 1. **`name` and `ordinal` must be written.** A constant allocated bare is
///    NON-NULL with a null `name()`, so every null check passes while
///    `Enum.valueOf` matches nothing, `toString()` is null and `compareTo`
///    calls every pair equal. That exact defect (a nameless enum constant)
///    zeroed fifteen netty classes earlier this session, so this is the trap
///    being avoided rather than a hypothetical.
/// 2. **The slots are resolved against `java/lang/Enum`, never the receiver's
///    class.** `Enum` declares `name` then `ordinal` and inherited fields come
///    first in the layout, so 0/1 hold for any subclass whatever fields IT
///    declares — and an enum that declares its OWN `name` field would shadow
///    `Enum`'s if resolution were receiver-scoped (`lang_misc::native_enum_name`
///    records what that cost in Spring Boot). The fallbacks `0`/`1` match
///    `lang_misc`'s `ENUM_NAME_SLOT`/`ENUM_ORDINAL_SLOT`, which is what
///    `Enum.name()`/`ordinal()` actually read; getting the fallback pair the
///    wrong way round produces exactly the nameless constant of (1).
/// 3. **`$VALUES` is re-READ out of the statics**, not filled from the refs
///    minted in pass one: `new_ref_array` allocates and can move them. This is
///    also what makes `values()[i] == CONSTANT` — the identity `Enum.valueOf`,
///    `Class.getEnumConstants` and `EnumSet` all rely on.
///
/// Two guards, and why each is the one used:
///
/// * `is_class_synthetic_stub` — only ever mint into a class THIS VM
///   fabricated. In real-JDK mode `java.net.http.HttpClient$Version` is a real
///   class with real `<clinit>` bytecode; `vm_util` prefers a class's own
///   `<clinit>` over the registry, but native-vs-bytecode dispatch elsewhere in
///   this VM is not uniformly bytecode-first, and clobbering the real JDK's
///   interned constants with synthetic stand-ins would be a regression in the
///   mode that matters most. The cost is that `MockNativeContext` answers
///   `false`, so this body cannot be driven from a Rust unit test (see the
///   record's nomination for the mock).
/// * an **idempotence** check on the first constant — `<clinit>` is invoked
///   once per class by construction, but `nio_file`'s equivalent carries the
///   same guard and a second entry through any path must not replace live
///   constants with fresh objects that fail `==`.
fn http_enum_clinit(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    constants: &[&str],
) -> MethodCallResult {
    if !ctx.is_class_synthetic_stub(class_name) {
        return Ok(None);
    }
    let Some(cid) = ctx.class_id_by_name(class_name) else {
        return Ok(None);
    };
    if let Some(first) = constants.first() {
        if let Some(slot) = ctx.static_field_index_by_name(cid, first) {
            if matches!(ctx.get_static_field(cid, slot), Value::Object(Some(_))) {
                return Ok(None);
            }
        }
    }
    let _ = ctx.ensure_class_initialized("java/lang/Enum");
    let name_slot = ctx
        .resolve_field_index("java/lang/Enum", "name")
        .unwrap_or(0);
    let ordinal_slot = ctx
        .resolve_field_index("java/lang/Enum", "ordinal")
        .unwrap_or(1);
    for (ordinal, name) in constants.iter().enumerate() {
        let obj = try_alloc_concurrent_synthetic(ctx, class_name, 2)?;
        // Pin across `create_string` — a moving young GC there would relocate
        // the fresh constant (native stale-local family). Nothing allocates
        // between the field writes and the publish to the static, which is a
        // GC root, so the constant is never unreachable-but-live.
        let obj_pin = ctx.pin_native_root(obj);
        let name_str = ctx.create_string(name);
        let obj = ctx.read_native_pin(obj_pin, obj);
        ctx.unpin_native_roots(obj_pin);
        ctx.set_field(obj, name_slot, Value::Object(Some(name_str)));
        // Cast: a constant count is far inside `i32`.
        ctx.set_field(obj, ordinal_slot, Value::Int(ordinal as i32));
        ctx.set_static_field_by_name(class_name, name, Value::Object(Some(obj)));
    }
    let values_array = ctx.new_ref_array(cid, constants.len());
    for (idx, name) in constants.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        // `set_array_element` does not allocate, so `values_array` cannot move
        // underneath this loop.
        ctx.set_array_element(values_array, idx, published);
    }
    ctx.set_static_field_by_name(class_name, "$VALUES", Value::Object(Some(values_array)));
    Ok(None)
}

/// `values()` — a FRESH array each call, holding the interned constants.
///
/// Freshness is measured, not stylistic: on the oracle
/// `Version.values() != Version.values()` (the real method clones `$VALUES`),
/// while `values()[1] == Version.HTTP_2`. Handing back the `$VALUES` array
/// itself would let one caller's `values()[0] = null` corrupt every later one.
fn http_enum_values(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    constants: &[&str],
) -> MethodCallResult {
    let cid = ctx.ensure_class_initialized(class_name)?;
    let arr = ctx.new_ref_array(cid, constants.len());
    for (idx, name) in constants.iter().enumerate() {
        let published = match ctx.static_field_index_by_name(cid, name) {
            Some(slot) => ctx.get_static_field(cid, slot),
            None => Value::Object(None),
        };
        ctx.set_array_element(arr, idx, published);
    }
    Ok(Some(Value::Object(Some(arr))))
}

/// `valueOf(String)` — resolved THROUGH the static field, so the answer is the
/// same object `GETSTATIC` yields (`valueOf("HTTP_2") == HTTP_2`).
///
/// Both failure shapes are the oracle's, quoted from this host:
/// `valueOf(null)` → `NullPointerException: Name is null`;
/// `Redirect.valueOf("nope")` → `IllegalArgumentException: No enum constant
/// java.net.http.HttpClient.Redirect.nope` — note the nested class renders with
/// a DOT, so both `/` and `$` are replaced.
fn http_enum_value_of(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    constants: &[&str],
    args: &[Value],
) -> MethodCallResult {
    let requested = match args.first() {
        Some(Value::Object(Some(s))) => ctx.read_string(*s),
        _ => None,
    };
    let Some(requested) = requested else {
        return Err(RuntimeError::NullPointerException {
            message: Some("Name is null".to_string()),
        }
        .into());
    };
    let cid = ctx.ensure_class_initialized(class_name)?;
    if constants.contains(&requested.as_str()) {
        if let Some(slot) = ctx.static_field_index_by_name(cid, &requested) {
            let published = ctx.get_static_field(cid, slot);
            if matches!(published, Value::Object(Some(_))) {
                return Ok(Some(published));
            }
        }
    }
    Err(RuntimeError::IllegalArgumentException {
        message: format!(
            "No enum constant {}.{requested}",
            class_name.replace(['/', '$'], ".")
        ),
    }
    .into())
}

pub(crate) fn register_p60_http_client(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    let hc = "java/net/http/HttpClient";
    r.register(
        hc,
        "newHttpClient",
        "()Ljava/net/http/HttpClient;",
        p60_new_http_client,
    );
    r.register(
        hc,
        "newBuilder",
        "()Ljava/net/http/HttpClient$Builder;",
        p60_http_client_builder,
    );
    r.register(
        hc,
        "version",
        "()Ljava/net/http/HttpClient$Version;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(hc, "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        p60_http_send);
    r.register(hc, "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        p60_http_send_async);

    // HttpClient.Builder = 1-field (version=0)
    let hcb = "java/net/http/HttpClient$Builder";
    r.register(
        hcb,
        "version",
        "(Ljava/net/http/HttpClient$Version;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        hcb,
        "followRedirects",
        "(Ljava/net/http/HttpClient$Redirect;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        hcb,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpClient$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        hcb,
        "build",
        "()Ljava/net/http/HttpClient;",
        p60_new_http_client,
    );

    // HttpClient.Version / HttpClient.Redirect — the enum constants.
    //
    // These five were registered in the FIELD shape until 2026-08-13 — a
    // method NAME that is really a field name, and a FIELD descriptor where a
    // method descriptor belongs (the descriptor's quotes are elided below so a
    // `register(` sweep does not count this comment as a live row):
    //
    //     r.register(hcv, "HTTP_2", Ljava/net/http/HttpClient$Version;, ...)
    //
    // and were DEAD, for exactly the reason the `SelectionKey.OP_*` note at
    // the top of this file gives: the native registry is keyed on
    // `(class, method, descriptor)` and every lookup originates at an INVOKE
    // instruction, whose descriptor starts with '('. `GETSTATIC` never
    // consults it — traced again for this change across all three static-read
    // paths this VM has:
    //
    //   * `vm/src/runtime/interpreter/opcodes.rs` `Instruction::Getstatic`
    //     — resolve, JVMTI watchpoint, `System.out/err/in` intercept, a
    //       `java/lang/Boolean` `TRUE`/`FALSE` special case, `get_static_shared`;
    //   * `vm/src/jit/helpers.rs` `jit_getstatic` — the same intercepts, then
    //     `get_static_shared`;
    //   * `jit/src/x64.rs` / `ir_lower.rs` `emit_inline_getstatic` — bakes the
    //     statics-base address and emits two loads, no helper call at all.
    //
    // None of the three has a registry lookup, so `HttpClient.Version.HTTP_2`
    // pushed the prepared default (null) and every builder downstream received
    // `Object(None)`.
    //
    // The shape the VM DOES support for a static constant is a native
    // `<clinit>`: `vm/src/vm/vm_util.rs` consults the registry for
    // `("<clinit>", "()V")` when the class itself declares no `<clinit>`, and
    // class initialization invokes it exactly once, after which plain
    // `getstatic` reads the published statics. The working models are
    // `stack_walker.rs`'s `native_option_clinit` (`StackWalker$Option`) and
    // `phases_late/nio_file.rs`'s `posix_file_permission_stub_clinit`.
    //
    // `values()` and `valueOf(String)` are registered alongside because javac
    // emits `invokestatic` for both against the ENUM class, and a synthetic
    // stub declares neither — `Version.values()` was a `NoSuchMethodError`.
    let hcv = HTTP_CLIENT_VERSION;
    r.register(hcv, "<clinit>", "()V", |ctx, _args| {
        http_enum_clinit(ctx, HTTP_CLIENT_VERSION, HTTP_VERSION_CONSTANTS)
    });
    r.register(
        hcv,
        "values",
        "()[Ljava/net/http/HttpClient$Version;",
        |ctx, _args| http_enum_values(ctx, HTTP_CLIENT_VERSION, HTTP_VERSION_CONSTANTS),
    );
    r.register(
        hcv,
        "valueOf",
        "(Ljava/lang/String;)Ljava/net/http/HttpClient$Version;",
        |ctx, args| http_enum_value_of(ctx, HTTP_CLIENT_VERSION, HTTP_VERSION_CONSTANTS, args),
    );

    let hcr = HTTP_CLIENT_REDIRECT;
    r.register(hcr, "<clinit>", "()V", |ctx, _args| {
        http_enum_clinit(ctx, HTTP_CLIENT_REDIRECT, HTTP_REDIRECT_CONSTANTS)
    });
    r.register(
        hcr,
        "values",
        "()[Ljava/net/http/HttpClient$Redirect;",
        |ctx, _args| http_enum_values(ctx, HTTP_CLIENT_REDIRECT, HTTP_REDIRECT_CONSTANTS),
    );
    r.register(
        hcr,
        "valueOf",
        "(Ljava/lang/String;)Ljava/net/http/HttpClient$Redirect;",
        |ctx, args| http_enum_value_of(ctx, HTTP_CLIENT_REDIRECT, HTTP_REDIRECT_CONSTANTS, args),
    );

    // HttpRequest = 3-field (uri=0, method=1, headers=2)
    let hr = "java/net/http/HttpRequest";
    r.register(
        hr,
        "newBuilder",
        "()Ljava/net/http/HttpRequest$Builder;",
        p60_http_request_builder,
    );
    r.register(
        hr,
        "newBuilder",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        p60_http_request_builder_uri,
    );
    r.register(hr, "uri", "()Ljava/net/URI;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(hr, "method", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // HttpRequest.Builder = 3-field (uri=0, method=1, headers=2)
    let hrb = "java/net/http/HttpRequest$Builder";
    r.register(
        hrb,
        "uri",
        "(Ljava/net/URI;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "GET",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("GET");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("POST");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("PUT");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "DELETE",
        "()Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = ctx.create_string("DELETE");
            ctx.set_field(this, 1, Value::Object(Some(m)));
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        hrb,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/HttpRequest$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        hrb,
        "timeout",
        "(Ljava/time/Duration;)Ljava/net/http/HttpRequest$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        hrb,
        "build",
        "()Ljava/net/http/HttpRequest;",
        p60_build_http_request,
    );

    // HttpRequest.BodyPublishers factory
    let bp = "java/net/http/HttpRequest$BodyPublishers";
    r.register(
        bp,
        "ofString",
        "(Ljava/lang/String;)Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1)?;
            ctx.set_field(obj, 0, args.first().copied().unwrap_or(Value::Object(None)));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        bp,
        "noBody",
        "()Ljava/net/http/HttpRequest$BodyPublisher;",
        |ctx, _args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$BodyPublisher", 1)?;
            ctx.set_field(obj, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // HttpResponse = 3-field (statusCode=0, body=1, headers=2)
    let resp = "java/net/http/HttpResponse";
    r.register(resp, "statusCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(resp, "body", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // HttpResponse.BodyHandlers factory
    let bh = "java/net/http/HttpResponse$BodyHandlers";
    r.register(
        bh,
        "ofString",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1)?;
            ctx.set_field(obj, 0, Value::Int(0)); // tag=0: string handler
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        bh,
        "discarding",
        "()Ljava/net/http/HttpResponse$BodyHandler;",
        |ctx, _args| {
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse$BodyHandler", 1)?;
            ctx.set_field(obj, 0, Value::Int(1)); // tag=1: discard
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

pub(crate) fn p60_new_http_client(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let client = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient", 1)?;
    ctx.set_field(client, 0, Value::Int(2)); // HTTP/2 default
    Ok(Some(Value::Object(Some(client))))
}

pub(crate) fn p60_http_client_builder(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let builder = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpClient$Builder", 1)?;
    ctx.set_field(builder, 0, Value::Int(2));
    Ok(Some(Value::Object(Some(builder))))
}

pub(crate) fn p60_http_request_builder(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    let builder = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 3)?;
    ctx.set_field(builder, 0, Value::Object(None));
    let method = ctx.create_string("GET");
    ctx.set_field(builder, 1, Value::Object(Some(method)));
    ctx.set_field(builder, 2, Value::Object(None));
    Ok(Some(Value::Object(Some(builder))))
}

pub(crate) fn p60_http_request_builder_uri(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let uri = args.first().copied().unwrap_or(Value::Object(None));
    let builder = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest$Builder", 3)?;
    ctx.set_field(builder, 0, uri);
    let method = ctx.create_string("GET");
    ctx.set_field(builder, 1, Value::Object(Some(method)));
    ctx.set_field(builder, 2, Value::Object(None));
    Ok(Some(Value::Object(Some(builder))))
}

pub(crate) fn p60_build_http_request(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let request = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpRequest", 3)?;
    ctx.set_field(request, 0, ctx.get_field(this, 0));
    ctx.set_field(request, 1, ctx.get_field(this, 1));
    ctx.set_field(request, 2, ctx.get_field(this, 2));
    Ok(Some(Value::Object(Some(request))))
}

pub(crate) fn p60_http_send(ctx: &mut dyn NativeContext, _args: &[Value]) -> MethodCallResult {
    // Return stub 200 OK response with empty body
    let response = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3)?;
    ctx.set_field(response, 0, Value::Int(200));
    let body = ctx.create_string("");
    ctx.set_field(response, 1, Value::Object(Some(body)));
    ctx.set_field(response, 2, Value::Object(None));
    Ok(Some(Value::Object(Some(response))))
}

pub(crate) fn p60_http_send_async(
    ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    // Return a CompletableFuture completed with stub response
    let response = try_alloc_concurrent_synthetic(ctx, "java/net/http/HttpResponse", 3)?;
    ctx.set_field(response, 0, Value::Int(200));
    let body = ctx.create_string("");
    ctx.set_field(response, 1, Value::Object(Some(body)));
    ctx.set_field(response, 2, Value::Object(None));
    let cf = p58_new_cf(ctx, Value::Object(Some(response)), true)?;
    Ok(Some(Value::Object(Some(cf))))
}

// =============================================================================
// AsynchronousFileChannel, AsynchronousSocketChannel, AsynchronousServerSocketChannel
// =============================================================================

pub(crate) fn register_p67_async_channels(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // AsynchronousFileChannel = 3-field (path_str=0, open=1, unused=2)
    let afc = "java/nio/channels/AsynchronousFileChannel";
    r.register(afc, "open", "(Ljava/nio/file/Path;[Ljava/nio/file/OpenOption;)Ljava/nio/channels/AsynchronousFileChannel;", |ctx, args| {
        let ch = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/AsynchronousFileChannel", 3)?;
        // Extract path string from the Path object (field 0 holds the string)
        let path_str_val = if let Some(Value::Object(Some(path_obj))) = args.first() {
            ctx.get_field(*path_obj, 0) // Path field 0 is the path string
        } else {
            Value::Object(None)
        };
        ctx.set_field(ch, 0, path_str_val); // store path string object
        ctx.set_field(ch, 1, Value::Int(1)); // open = true
        ctx.set_field(ch, 2, Value::Int(0));
        Ok(Some(Value::Object(Some(ch))))
    });
    r.register(
        afc,
        "read",
        "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;",
        |ctx, args| {
            // Real async file read: read from file into the ByteBuffer
            let this = obj_arg(args, 0)?;
            let bb = obj_arg(args, 1)?;
            let position = match args.get(2) {
                Some(Value::Long(l)) => *l as u64,
                _ => 0u64,
            };

            // Get path string from the channel object
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };

            // Get ByteBuffer position and limit
            let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize; // BB_POS
            let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize; // BB_LIMIT
            let remaining = if bb_lim > bb_pos { bb_lim - bb_pos } else { 0 };

            let bytes_read = if !path.is_empty() && remaining > 0 {
                use std::io::{Read, Seek, SeekFrom};
                match std::fs::File::open(&path) {
                    Ok(mut file) => {
                        let _ = file.seek(SeekFrom::Start(position));
                        let mut tmp = vec![0u8; remaining];
                        match file.read(&mut tmp) {
                            Ok(0) => -1i32, // EOF
                            Ok(n) => {
                                // Store bytes into the ByteBuffer backing array
                                if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
                                    // BB_ARRAY
                                    for i in 0..n {
                                        ctx.set_array_element(
                                            arr,
                                            bb_pos + i,
                                            Value::Int(tmp[i] as i8 as i32),
                                        );
                                    }
                                }
                                // Advance ByteBuffer position
                                ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
                                n as i32
                            }
                            Err(_) => -1,
                        }
                    }
                    Err(_) => -1,
                }
            } else {
                -1
            };

            // A completed real Future carrying the byte count. This used to be
            // a two-slot `java/util/concurrent/FutureTask` written by field
            // index -- on the real `java.base` class that puts the count in
            // `state:int`, and `FutureTask`'s constant block reads 2..=6 as
            // NORMAL/EXCEPTIONAL/CANCELLED/INTERRUPTING/INTERRUPTED, so a
            // 3-byte read would have reported itself as completed
            // EXCEPTIONALLY. This registration is overwritten by `native-io`'s
            // `native_afc_read` in every arm, so that never fired -- it was one
            // registration-order change away from firing, which is why it is
            // corrected rather than left. See `aio_completed_future`.
            let boxed = aio_box_int(ctx, bytes_read);
            aio_completed_future(ctx, boxed)
        },
    );
    r.register(
        afc,
        "write",
        "(Ljava/nio/ByteBuffer;J)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let _bb = args.get(1); // ByteBuffer (simplified: extract from position/limit)
            let position = match args.get(2) {
                Some(Value::Long(p)) => *p,
                _ => 0,
            };

            // Read path from field 0
            let path = match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };

            // Extract bytes from ByteBuffer: field 0 = backing byte[], field 1 = position, field 2 = limit
            let bytes_written = if !path.is_empty() {
                let bb_data: Vec<u8> = if let Some(Value::Object(Some(bb))) = args.get(1) {
                    // Read backing array from ByteBuffer field 0
                    match ctx.get_field(*bb, 0) {
                        Value::Object(Some(arr)) => {
                            let bb_pos = match ctx.get_field(*bb, 1) {
                                Value::Int(p) => p as usize,
                                _ => 0,
                            };
                            let bb_lim = match ctx.get_field(*bb, 2) {
                                Value::Int(l) => l as usize,
                                _ => ctx.array_length(arr),
                            };
                            let len = bb_lim.saturating_sub(bb_pos);
                            let mut buf = vec![0u8; len];
                            for i in 0..len {
                                if let Value::Int(b) = ctx.get_array_element(arr, bb_pos + i) {
                                    buf[i] = b as u8;
                                }
                            }
                            buf
                        }
                        _ => Vec::new(),
                    }
                } else {
                    Vec::new()
                };
                use std::io::Write;
                if let Ok(mut file) = std::fs::OpenOptions::new()
                    .write(true)
                    .create(true)
                    // `AsynchronousFileChannel.write(buf, position)` is a
                    // POSITIONAL write: it seeks to `position` below and
                    // overwrites that range, leaving the rest of the file
                    // intact. Truncating here would discard everything past
                    // the written range on every call — so state the `false`
                    // explicitly rather than leaning on the default, which is
                    // what `clippy::suspicious_open_options` asks for. This is
                    // a no-op at runtime: `truncate` already defaulted to
                    // false, so behaviour is unchanged.
                    .truncate(false)
                    .open(&path)
                {
                    use std::io::Seek;
                    let _ = file.seek(std::io::SeekFrom::Start(position as u64));
                    match file.write_all(&bb_data) {
                        Ok(()) => bb_data.len() as i32,
                        Err(_) => 0i32,
                    }
                } else {
                    0i32
                }
            } else {
                0i32
            };

            // Same correction as the sibling `read` above: a completed real
            // Future, not a field-index-written `java.util.concurrent.FutureTask`.
            let boxed = aio_box_int(ctx, bytes_written);
            aio_completed_future(ctx, boxed)
        },
    );
    r.register(afc, "size", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let path = match ctx.get_field(this, 0) {
            Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
            _ => return Ok(Some(Value::Long(0))),
        };
        let size = std::fs::metadata(&path)
            .map(|m| m.len() as i64)
            .unwrap_or(0);
        Ok(Some(Value::Long(size)))
    });
    r.register(
        afc,
        "truncate",
        "(J)Ljava/nio/channels/AsynchronousFileChannel;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    // force(boolean metaData)V -- the ONLY registrant of this triple in the
    // tree, live in both shipping modes. `native-io` does not declare it, so
    // nothing overwrites this body the way it overwrites the ten AFC triples
    // around it.
    //
    // WHAT THIS USED TO DO: nothing at all, on every channel this VM produces.
    // It read slot 0 as a path `String` under THIS file's stale
    // `path_str=0, open=1` belief. Every `AsynchronousFileChannel` in the VM is
    // allocated by `native-io`'s `alloc_afc_channel`, whose layout is
    // `AFC_FIELD_FD = 0` (an `Int`), `AFC_FIELD_PATH = 1`, `AFC_FIELD_OPEN = 2`
    // -- so the `match` took its `_ =>` arm and returned `Ok(None)` BEFORE it
    // ever looked at its `metaData` argument. `force(true)`, a durability
    // barrier (H2's `FileAsync` is a caller -- see the sibling `write` body's
    // comment), returned normally having flushed nothing. Nothing errored; the
    // corruption only appears after a crash.
    //
    // Three further defects were stacked behind the slot error and are fixed
    // with it: the sync_all/sync_data polarity was inverted relative to
    // `AsynchronousFileChannel.force(boolean metaData)` ("true ... content AND
    // metadata"), the fsync was issued on a SECOND descriptor opened by path
    // (which carries none of this channel's buffered writes, and whose
    // `.write(true)` open fails outright on a read-only channel), and there was
    // no closed-channel refusal where the javadoc says
    // `@throws ClosedChannelException`.
    //
    // The layout constants are spelled literally here because they belong to
    // the other crate; `cratonvm_native_io::afc_sync_at` is the half that has
    // to know the handle table. The rule this body got wrong is that the
    // registration which decides the layout is the one that ALLOCATES.
    //
    // REGISTERED ON THE CONCRETE CLASSES TOO, 2026-08-21 (WORKER 4), and that
    // is not optional. `native-io`'s `alloc_afc_channel` now mints
    // `sun.nio.ch.SimpleAsynchronousFileChannelImpl` (Linux) /
    // `…Windows…` instead of an instance of the ABSTRACT
    // `java.nio.channels.AsynchronousFileChannel` (JVMS 6.5 -- see
    // `probes/W4Abstract.java`). Dispatch keys on the receiver's runtime class
    // (`H11-1`), so an abstract-only row stops being reached the moment the
    // mint moves. MEASURED: with the row on the abstract name only,
    // `RJdkAsyncChannel` died with
    //   NullPointerException: Cannot invoke "sun.nio.ch.NativeThreadSet.add()"
    //   because "this.threads" is null
    //     at sun/nio/ch/SimpleAsynchronousFileChannelImpl.implForce
    // — the JDK's own `force` bytecode, running against a channel whose
    // `<init>` this VM never ran. `scripts/…`-free reproduction: the
    // cross-crate audit in this lane's record lists every triple in that
    // position; `force` and `AsynchronousSocketChannel.connect` were the two.
    //
    // AND THE SLOT INDICES ARE GONE. This body read `get_field(this, 2)` and
    // `get_field(this, 0)` under a hard-coded copy of the other crate's map.
    // Since 2026-08-21 that map is APPENDED above whatever the concrete class
    // declares, so no constant can be right. `afc_channel_is_open` /
    // `afc_channel_handle_id` are the allocator's own readers, published for
    // this call site — which is the rule the comment above states.
    let force_body = |ctx: &mut dyn cratonvm_native_api::NativeContext,
                      args: &[Value]|
     -> cratonvm_types::error::MethodCallResult {
        let this = obj_arg(args, 0)?;
        if !cratonvm_native_io::afc_channel_is_open(ctx, this) {
            return Err(cratonvm_native_io::afc_closed_channel_error(ctx));
        }
        // `metaData == true` is "content AND metadata" -> sync_all.
        let metadata = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let handle_id = match cratonvm_native_io::afc_channel_handle_id(ctx, this) {
            Some(id) => id,
            None => return Ok(None),
        };
        // STW-TAKEOVER guard, same as the `afc_truncate_at` call site in
        // `native_afc_truncate`: `afc_sync_at` parks on a real `Mutex::lock()`
        // and then issues `fsync`/`FlushFileBuffers`, which on a dirty file is
        // genuinely unbounded disk I/O. Without the bracket a concurrent STW
        // pause counts this thread as an ordinary cooperating mutator and waits
        // for a safepoint it cannot reach while parked -- the hang shape H2's
        // `TestFileSystem.testConcurrent` on the `async:` filesystem already
        // produced once for the read path. `this` is not touched after the
        // call, so the ref-resyncing form is not needed.
        ctx.begin_blocking_region();
        let sync_result = cratonvm_native_io::afc_sync_at(handle_id, metadata);
        ctx.end_blocking_region();
        sync_result.map_err(|e| RuntimeError::IOException {
            message: format!("AsynchronousFileChannel.force: {e}"),
        })?;
        Ok(None)
    };
    for cls in std::iter::once(afc)
        .chain(cratonvm_native_io::AFC_IMPLS.iter().copied())
        .chain(std::iter::once(cratonvm_native_io::AFC_ABSTRACT_IMPL))
    {
        r.register(cls, "force", "(Z)V", force_body);
    }
    // `lock()Ljava/util/concurrent/Future;` was registered here and is now
    // registered by `native-io`'s `register_async_file_channel`
    // (`native_afc_lock`), beside the `tryLock(JJZ)` whose real
    // `sun/nio/ch/FileLockImpl` plumbing it reuses. The body deleted here minted
    // `try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/FutureTask", 2)`
    // and wrote two slots by index -- `FutureTask` is a real `java.base` class
    // whose slot 0 is `state:int` and slot 1 is `callable`, so the future came
    // out at state NEW and its real `get()` parked forever. Deleted rather than
    // corrected in place, because the correction needs the channel's handle
    // table, which lives in the crate that allocates the channel.
    r.register(afc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(0));
        Ok(None)
    });
    r.register(afc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    // AsynchronousSocketChannel = 4-field (connected=0, open=1, fd_id=2, remote_addr=3)
    //
    // DEAD, EXCEPT ONE TRIPLE, AND THE SURVIVOR DISAGREES WITH ITS OWNER
    // (measured 2026-08-12, lane W7-49 — W7-49-slot-index-recensus.md).
    //
    // `native-io/src/async_socket.rs::register_async_socket_real` registers
    // `open()` x2, `isOpen`, `close`, `getRemoteAddress`, `read(ByteBuffer)` and
    // `write(ByteBuffer)` on this same class, and `register_io_natives` runs
    // AFTER `register_essential_natives_with_shims` (vm_init.rs:1701 then 1898,
    // and 2208 then 2403). Registration is last-write-wins, so every one of
    // those registrations below is overwritten and never dispatches.
    //
    // The one survivor is `connect(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;`
    // — native-io registers only the `(SocketAddress, Object, CompletionHandler)V`
    // form, so this descriptor is not overwritten. It therefore runs against
    // objects `aio_asc_open` allocated, under a DIFFERENT slot map:
    //
    //     here            native-io (the owner, and the allocator)
    //     0 connected     0 F_OPEN
    //     1 open          1 F_CONNECTED
    //     2 fd_id         2 F_REG_ID   (an AIO registry id, NOT an fd_table fd)
    //     3 remote        3 F_REMOTE
    //
    // Slots 0 and 1 have OPPOSITE meanings and slot 2 holds a different KIND of
    // integer. That is the two-layouts-on-one-class condition — the shape that
    // made `java.lang.Process` a bug.
    //
    // REPAIRED 2026-08-21 (WORKER 4), in the one step this comment said it
    // needed: the surviving `connect` body below no longer indexes slots at
    // all, it calls `cratonvm_native_io::async_socket::async_socket_note_connected`,
    // so there is exactly one map and its owner holds it. The two `open`
    // bodies below still write 0..3 by index and are still DEAD (overwritten
    // by `native-io`'s registrations, which run later); they are left in place
    // rather than deleted because retiring a loser is a registry change that
    // wants its own `--dump-native-registry` before/after, and this lane's
    // change is behavioural.
    //
    // Neither map is layout-correct either way: real
    // `java.nio.channels.AsynchronousSocketChannel` (JDK 25.0.3.9, `javap -p`)
    // declares exactly ONE instance field, `provider`, a reference the collector
    // scans as an oop — so slot 0 of both maps writes an `Int` into it and slots
    // 1-3 sit past the end of the real layout. `alloc_obj(..., N_FIELDS)` on the
    // native-io side is a DIRECT allocation, so it never reaches
    // `report_layout_alias` and this pair appears in no run of that census.
    let asc = "java/nio/channels/AsynchronousSocketChannel";
    r.register(
        asc,
        "open",
        "()Ljava/nio/channels/AsynchronousSocketChannel;",
        |ctx, _args| {
            let ch = try_alloc_concurrent_synthetic(
                ctx,
                "java/nio/channels/AsynchronousSocketChannel",
                4,
            )?;
            ctx.set_field(ch, 0, Value::Int(0)); // not connected
            ctx.set_field(ch, 1, Value::Int(1)); // open
            ctx.set_field(ch, 2, Value::Int(-1)); // no fd
            ctx.set_field(ch, 3, Value::Object(None)); // remote addr
            Ok(Some(Value::Object(Some(ch))))
        },
    );
    r.register(
        asc,
        "open",
        "(Ljava/nio/channels/AsynchronousChannelGroup;)Ljava/nio/channels/AsynchronousSocketChannel;",
        |ctx, _args| {
            let ch =
                try_alloc_concurrent_synthetic(ctx, "java/nio/channels/AsynchronousSocketChannel", 4)?;
            ctx.set_field(ch, 0, Value::Int(0));
            ctx.set_field(ch, 1, Value::Int(1));
            ctx.set_field(ch, 2, Value::Int(-1));
            ctx.set_field(ch, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(ch))))
        },
    );
    // ON THE CONCRETE CLASSES TOO, 2026-08-21 (WORKER 4). Same reason as
    // `force(Z)V` above, and found by the same cross-crate audit:
    // `native-io`'s `aio_asc_open` now mints
    // `sun.nio.ch.UnixAsynchronousSocketChannelImpl` rather than an instance
    // of the ABSTRACT public class (JVMS 6.5 -- `probes/W4Abstract.java`),
    // dispatch keys on the receiver (`H11-1`), and this is the ONLY
    // registration of this triple in the tree. An abstract-only row would
    // therefore stop answering the moment the mint moved, and the JDK's own
    // `connect` bytecode would run against a channel with no initialised
    // state -- the exact shape `RJdkAsyncChannel` hit for `force`.
    //
    // Typed as `NativeCallback` (a plain `fn` pointer) so the non-capturing
    // closure coerces once and can be handed to `register` per class.
    let connect_body: cratonvm_native_api::registry::NativeCallback = |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Extract host:port from the SocketAddress. The argument is a real
        // `InetSocketAddress` (state behind a private `holder`), NOT a flat
        // synthetic — reading slot 0/1 directly yielded a bogus host/port and
        // the connect failed with WSAEADDRNOTAVAIL (os error 10049). Use the
        // holder-aware reader shared with java.net.Socket.connect.
        let addr_str = if let Some(Value::Object(Some(sa))) = args.get(1) {
            let (host, port) = crate::net_phase_e::read_inet_socket_address(ctx, *sa)?;
            format!("{}:{}", host, port)
        } else {
            "127.0.0.1:80".into()
        };
        // Blocking TCP connect
        let fd_id =
            crate::capability_gate::open_tcp_connect_gated(&*ctx, &addr_str).map_err(|e| {
                crate::capability_gate::translate_open_failure(e, |io| {
                    format!("connect failed: {io}")
                })
            })?;
        // FIXED 2026-08-21 (WORKER 4). These three lines used to write
        // slots 0/2/3 BY INDEX, under the map documented above -- a map
        // that disagrees with the OWNER's on slots 0 and 1 and on what
        // kind of integer slot 2 holds. The receiver is always an object
        // `native-io`'s `aio_asc_open` allocated (that crate wins every
        // other triple on this class), and since 2026-08-21 its private
        // map is APPENDED above the concrete `sun.nio.ch.*Impl` layout,
        // so an absolute index here is now wrong twice over.
        //
        // One writer, one map: this calls the owner's own setter. The
        // repair W7-49 said "belongs to a lane that owns native-io" --
        // that lane also owns this line, so both halves move together, as
        // that record required.
        let remote = args.get(1).copied().unwrap_or(Value::Object(None));
        cratonvm_native_io::async_socket::async_socket_note_connected(
            ctx,
            this,
            fd_id as i32,
            remote,
        );
        // DF07: completed Future<Void> via real CompletableFuture (see helper).
        aio_completed_future(ctx, Value::Object(None))
    };
    for cls in std::iter::once(asc)
        .chain(cratonvm_native_io::async_socket::ASC_IMPLS.iter().copied())
        .chain(std::iter::once(
            cratonvm_native_io::async_socket::ASC_ABSTRACT_IMPL,
        ))
    {
        r.register(
            cls,
            "connect",
            "(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;",
            connect_body,
        );
    }
    r.register(
        asc,
        "read",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 2).as_int().unwrap_or(-1);
            let bb = obj_arg(args, 1)?;
            // DF07: decode the destination buffer via the real-or-synthetic
            // accessor (a real HeapByteBuffer's array is `hb`, not slot 0).
            // GC-blocking audit — see the matching comment on the sibling
            // `write` registration below for the full rationale (found via
            // the same STW-takeover-cluster residual investigation).
            let bytes_read = if fd_id >= 0 {
                match aio_bb_region(ctx, bb) {
                    Some((arr, off, remaining)) if remaining > 0 => {
                        let mut tmp = vec![0u8; remaining];
                        // STW-TAKEOVER-FIX (2026-07-14): tcp_read performs a
                        // genuinely blocking OS recv() (real socket I/O
                        // faked up as "async" via an already-completed
                        // Future, not offloaded to a worker thread) with no
                        // bound on wait time. Without a GC-barrier blocking
                        // region, a concurrent STW pause counts this thread
                        // as an ordinary cooperating mutator and waits
                        // forever for it to reach a safepoint it can never
                        // reach while parked in recv() — this is the
                        // `pending=1`/`taken=0` permanent
                        // "STW cross-thread JIT takeover" hang confirmed via
                        // live gdb (thread blocked in
                        // native-api/src/fd_table.rs's `tcp_read` ->
                        // std::net::TcpStream::read, no frame trace, not
                        // recognized as GC-safe) in
                        // web.socket.messaging.StompWebSocketIntegrationTests's
                        // `sendMessageToBrokerAndReceiveInOrder`. `arr`/`bb`
                        // are ObjectRefs used again after the call returns,
                        // so they go through `end_blocking_region_refs` to
                        // pick up any relocation from a GC that ran while
                        // blocked.
                        let mut blocked_refs = [Value::Object(Some(arr)), Value::Object(Some(bb))];
                        ctx.begin_blocking_region();
                        let read_result = ctx.fd_table().tcp_read(fd_id as u32, &mut tmp);
                        ctx.end_blocking_region_refs(&mut blocked_refs);
                        let arr = match blocked_refs[0] {
                            Value::Object(Some(o)) => o,
                            _ => arr,
                        };
                        let bb = match blocked_refs[1] {
                            Value::Object(Some(o)) => o,
                            _ => bb,
                        };
                        match read_result {
                            Ok(0) => -1,
                            Ok(n) => {
                                ctx.write_byte_array_from(arr, off, &tmp[..n]);
                                aio_bb_advance(ctx, bb, n as i32);
                                n as i32
                            }
                            Err(_) => -1,
                        }
                    }
                    _ => 0,
                }
            } else {
                -1
            };
            let boxed = aio_box_int(ctx, bytes_read);
            aio_completed_future(ctx, boxed)
        },
    );
    r.register(
        asc,
        "write",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/Future;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 2).as_int().unwrap_or(-1);
            let bb = obj_arg(args, 1)?;
            // DF07: source the bytes via the real-or-synthetic accessor (see read).
            //
            // GC-blocking audit (STW takeover 5-class cluster residual,
            // 2026-07-13, Azure host follow-up): `tcp_write` is a raw
            // blocking `send()` with NO GC-blocking-region bracket — found
            // via gdb on a hung TestWsWebSocketContainerTimeoutClient (the
            // test deliberately never drains the peer socket to force a
            // write timeout, so this send() parks in the OS indefinitely
            // once the send buffer fills). Bracketing it fixes the
            // STW-takeover hang (this thread was counted in `expected`
            // forever, `taken=0`, matching the doc's signature exactly).
            //
            // NOTE this does NOT fix the deeper issue that this Future-
            // returning overload is synchronous (blocks the calling thread
            // until the write completes or errors) rather than genuinely
            // async like the sibling CompletionHandler-based overload in
            // native-io/src/async_socket.rs's aio_asc_write (which
            // dispatches to a worker pool via Job::Write and returns
            // immediately with a real pending Future). A caller doing
            // `write(bb).get(timeout, unit)` to detect a write timeout will
            // still block inside this native call itself rather than inside
            // `Future.get`, so the write always "succeeds" eventually (once
            // the peer reads or the connection resets) instead of the
            // caller's own timeout ever firing — a separate, out-of-scope-
            // for-this-fix architectural gap. Filed as a residual; see
            // stw-crossthread-jit-takeover-hang-cluster.md.
            let bytes_written = if fd_id >= 0 {
                match aio_bb_region(ctx, bb) {
                    Some((arr, off, remaining)) if remaining > 0 => {
                        let mut data = vec![0u8; remaining];
                        ctx.read_byte_array_into(arr, off, &mut data);
                        // STW-TAKEOVER-FIX (2026-07-14): see the `read`
                        // registration above — tcp_write is the same
                        // genuinely-blocking-socket-call-with-no-GC-barrier-
                        // registration shape (the write-side counterpart of
                        // the same missing-safepoint-cooperation gap). `arr`
                        // was already fully consumed above (copied into the
                        // Rust-owned `data` buffer), so only `bb` needs to
                        // survive the blocking window.
                        let mut blocked_refs = [Value::Object(Some(bb))];
                        ctx.begin_blocking_region();
                        let write_result = ctx.fd_table().tcp_write(fd_id as u32, &data);
                        ctx.end_blocking_region_refs(&mut blocked_refs);
                        let bb = match blocked_refs[0] {
                            Value::Object(Some(o)) => o,
                            _ => bb,
                        };
                        match write_result {
                            Ok(n) => {
                                aio_bb_advance(ctx, bb, n as i32);
                                n as i32
                            }
                            Err(_) => -1,
                        }
                    }
                    _ => 0,
                }
            } else {
                -1
            };
            let boxed = aio_box_int(ctx, bytes_written);
            aio_completed_future(ctx, boxed)
        },
    );
    r.register(asc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 2).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().close(fd_id as u32);
        }
        ctx.set_field(this, 1, Value::Int(0)); // closed
        ctx.set_field(this, 2, Value::Int(-1));
        Ok(None)
    });
    r.register(asc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });
    r.register(
        asc,
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 3)))
        },
    );

    // AsynchronousServerSocketChannel
    let assc = "java/nio/channels/AsynchronousServerSocketChannel";
    r.register(
        assc,
        "open",
        "()Ljava/nio/channels/AsynchronousServerSocketChannel;",
        |ctx, _args| {
            let ch = try_alloc_concurrent_synthetic(
                ctx,
                "java/nio/channels/AsynchronousServerSocketChannel",
                1,
            )?;
            ctx.set_field(ch, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(ch))))
        },
    );
    r.register(
        assc,
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/AsynchronousServerSocketChannel;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    // `accept()Ljava/util/concurrent/Future;` was registered here and is
    // DELETED, not corrected. It was the sole registrant -- `native-io`'s
    // `async_socket.rs` registers only the
    // `accept(Ljava/lang/Object;Ljava/nio/channels/CompletionHandler;)V` form --
    // and the body accepted NOTHING: it minted a two-slot
    // `java/util/concurrent/FutureTask` and set slot 0 (`state:int` on the real
    // `java.base` class) to `Object(None)`, i.e. `Int(0)` = NEW, and slot 1
    // (`callable`) to `Int(0)`. Real `FutureTask.get()` then parks forever and
    // `isDone()` is permanently false, so every caller either hangs or reads a
    // future that never becomes ready.
    //
    // Neither remedy available inside this file is a fix. Completing it with
    // `aio_completed_future(ctx, Value::Object(None))` hands back a
    // `Future<AsynchronousSocketChannel>` that completes with `null`, which the
    // caller dereferences -- a quiet failure traded for a loud one. Doing the
    // accept for real needs the listener that `aio_assc_open`/`aio_assc_bind`
    // registered in `native-io/src/async_socket.rs`, in that crate's own slot
    // map, which is where the working `accept(Object,CompletionHandler)` half
    // already lives. So the registration is removed and the real JDK's abstract
    // `accept()` becomes an `AbstractMethodError` at the call site: a loud
    // failure naming the missing method, instead of a thread that never wakes.
    // The real implementation belongs beside its CompletionHandler twin.
    r.register(assc, "close", "()V", |ctx, args| {
        // AsynchronousServerSocketChannel = 1-field (open=0). Mark as closed.
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Int(0));
        }
        Ok(None)
    });

    // AsynchronousChannelGroup
    let acg = "java/nio/channels/AsynchronousChannelGroup";
    r.register(
        acg,
        "withThreadPool",
        "(Ljava/util/concurrent/ExecutorService;)Ljava/nio/channels/AsynchronousChannelGroup;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(
                ctx,
                "java/nio/channels/AsynchronousChannelGroup",
                1,
            )?;
            ctx.set_field(obj, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        acg,
        "withFixedThreadPool",
        "(ILjava/util/concurrent/ThreadFactory;)Ljava/nio/channels/AsynchronousChannelGroup;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(
                ctx,
                "java/nio/channels/AsynchronousChannelGroup",
                1,
            )?;
            ctx.set_field(obj, 0, Value::Int(1));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(acg, "shutdown", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Int(0)); // 0 = shut down
        }
        Ok(None)
    });
    r.register(acg, "shutdownNow", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            ctx.set_field(this, 0, Value::Int(0));
        }
        Ok(None)
    });
    r.register(acg, "isShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            let state = ctx.get_field(this, 0).as_int().unwrap_or(1);
            Ok(Some(Value::Int(if state == 0 { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    r.register(acg, "isTerminated", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > 0 {
            let state = ctx.get_field(this, 0).as_int().unwrap_or(1);
            Ok(Some(Value::Int(if state == 0 { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    });
    // awaitTermination(timeout, unit) — the constant `true` here claimed the
    // group had terminated even when `shutdown()` was never called, so a
    // caller's "did my I/O drain?" check always passed. Report the real state
    // (field 0 == 0 means shut down, same encoding `isTerminated` reads) and
    // honour the timeout by polling for it.
    //
    // MAY BLOCK: bounded by the caller's own timeout, inside a timed blocking
    // region so a stop-the-world GC does not queue up behind the wait.
    r.register(
        acg,
        "awaitTermination",
        "(JLjava/util/concurrent/TimeUnit;)Z",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            let timeout = match args.get(1) {
                Some(Value::Long(v)) => *v,
                Some(Value::Int(v)) => *v as i64,
                _ => 0,
            };
            // TimeUnit -> milliseconds via the unit's own bytecode; a failed
            // upcall degrades to "treat the number as milliseconds", never to
            // an unbounded wait.
            let timeout_ms = match args.get(2) {
                Some(Value::Object(Some(unit))) => {
                    match ctx.invoke_virtual(*unit, "toMillis", "(J)J", &[Value::Long(timeout)]) {
                        Ok(Some(Value::Long(ms))) => ms,
                        _ => timeout,
                    }
                }
                _ => timeout,
            };
            fn terminated(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
                ctx.object_num_fields(this) > 0 && ctx.get_field(this, 0).as_int().unwrap_or(1) == 0
            }
            if timeout_ms <= 0 || terminated(ctx, this) {
                let done = terminated(ctx, this);
                return Ok(Some(Value::Int(if done { 1 } else { 0 })));
            }
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms as u64);
            loop {
                if terminated(ctx, this) {
                    return Ok(Some(Value::Int(1)));
                }
                if std::time::Instant::now() >= deadline {
                    return Ok(Some(Value::Int(0)));
                }
                ctx.begin_timed_blocking_region();
                std::thread::sleep(std::time::Duration::from_millis(1));
                let mut refs = [Value::Object(Some(this))];
                ctx.end_blocking_region_refs(&mut refs);
                if let Value::Object(Some(moved)) = refs[0] {
                    this = moved;
                }
            }
        },
    );

    // =========================================================================
    // Pipe channels — in-memory pipe via shared VecDeque
    // Pipe = 2-field (source=0 SourceChannel, sink=1 SinkChannel)
    // SourceChannel = 3-field (open=0, fd_id=1, blocking=2)
    // SinkChannel = 3-field (open=0, fd_id=1, blocking=2)
    // =========================================================================
    let pipe = "java/nio/channels/Pipe";
    r.register(pipe, "open", "()Ljava/nio/channels/Pipe;", |ctx, _args| {
        let (read_fd, write_fd) = ctx.fd_table().open_pipe();

        let source =
            try_alloc_concurrent_synthetic(ctx, "java/nio/channels/Pipe$SourceChannel", 3)?;
        ctx.set_field(source, 0, Value::Int(1)); // open
        ctx.set_field(source, 1, Value::Int(read_fd as i32));
        ctx.set_field(source, 2, Value::Int(1)); // blocking

        let sink = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/Pipe$SinkChannel", 3)?;
        ctx.set_field(sink, 0, Value::Int(1)); // open
        ctx.set_field(sink, 1, Value::Int(write_fd as i32));
        ctx.set_field(sink, 2, Value::Int(1)); // blocking

        let p = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/Pipe", 2)?;
        ctx.set_field(p, 0, Value::Object(Some(source)));
        ctx.set_field(p, 1, Value::Object(Some(sink)));
        Ok(Some(Value::Object(Some(p))))
    });
    r.register(
        pipe,
        "source",
        "()Ljava/nio/channels/Pipe$SourceChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        },
    );
    r.register(
        pipe,
        "sink",
        "()Ljava/nio/channels/Pipe$SinkChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 1)))
        },
    );

    // SourceChannel.read(ByteBuffer)
    let src_ch = "java/nio/channels/Pipe$SourceChannel";
    r.register(src_ch, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        if fd_id < 0 || ctx.get_field(this, 0).as_int().unwrap_or(0) == 0 {
            return Err(RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into());
        }
        let bb = obj_arg(args, 1)?;
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut tmp = vec![0u8; remaining];
        let n = ctx
            .fd_table()
            .pipe_read(fd_id as u32, &mut tmp)
            .map_err(|e| RuntimeError::IOException {
                message: format!("pipe read: {}", e),
            })?;
        if n == 0 {
            return Ok(Some(Value::Int(-1))); // EOF
        }
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for i in 0..n {
                ctx.set_array_element(arr, bb_pos + i, Value::Int(tmp[i] as i8 as i32));
            }
        }
        ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    });
    r.register(src_ch, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().close(fd_id as u32);
        }
        ctx.set_field(this, 0, Value::Int(0));
        Ok(None)
    });
    r.register(src_ch, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        src_ch,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // SinkChannel.write(ByteBuffer)
    let sink_ch = "java/nio/channels/Pipe$SinkChannel";
    r.register(sink_ch, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        if fd_id < 0 || ctx.get_field(this, 0).as_int().unwrap_or(0) == 0 {
            return Err(RuntimeError::IOException {
                message: "Channel closed".into(),
            }
            .into());
        }
        let bb = obj_arg(args, 1)?;
        let bb_pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let bb_lim = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = bb_lim.saturating_sub(bb_pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut data = vec![0u8; remaining];
        if let Value::Object(Some(arr)) = ctx.get_field(bb, 0) {
            for i in 0..remaining {
                data[i] = ctx.get_array_element(arr, bb_pos + i).as_int().unwrap_or(0) as u8;
            }
        }
        let n = ctx
            .fd_table()
            .pipe_write(fd_id as u32, &data)
            .map_err(|e| RuntimeError::IOException {
                message: format!("pipe write: {}", e),
            })?;
        ctx.set_field(bb, 1, Value::Int((bb_pos + n) as i32));
        Ok(Some(Value::Int(n as i32)))
    });
    r.register(sink_ch, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 1).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().close(fd_id as u32);
        }
        ctx.set_field(this, 0, Value::Int(0));
        Ok(None)
    });
    r.register(sink_ch, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.register(
        sink_ch,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );

    // NOTE: `java.net.UnixDomainSocketAddress.of()`/`getPath()` used to be
    // registered here as natives that unconditionally threw
    // `UnsupportedOperationException("Unix domain sockets are not supported on
    // this platform")`. That override also applied in real-JDK mode, where the
    // genuine `java.net.UnixDomainSocketAddress` is a plain (non-native) class
    // whose bytecode works fine on CratonVM — it only needs `Path.of` and
    // `FileSystems.getDefault()`, both of which are supported. The stub was
    // what failed Tomcat's `TestXxxEndpoint.testUnixDomainSocket`
    // ("Protocol handler initialization failed" from
    // `NioEndpoint.initServerSocket`). Unix-domain sockets are now implemented
    // for real — see `native-io/src/uds.rs` plus the `open(ProtocolFamily)` /
    // `bind` / `accept` / `connect` handling in
    // `native-io/src/socket_channel.rs` — so the address class is deliberately
    // left to its own bytecode and no registration belongs here.
    r.set_category(__prev_cat);
}

// =============================================================================
// java.net.http.WebSocket — Java 11
// =============================================================================

// WebSocket field layout: 5-field
// 0=fd_id (Int), 1=output_closed (Int), 2=input_closed (Int), 3=subprotocol (String), 4=state (Int 0=open,1=closing,2=closed)
pub(crate) const P69_WS_FD: usize = 0;

pub(crate) const P69_WS_OUT_CLOSED: usize = 1;

pub(crate) const P69_WS_IN_CLOSED: usize = 2;

pub(crate) const P69_WS_SUBPROTO: usize = 3;

pub(crate) const P69_WS_STATE: usize = 4;

pub(crate) const P69_WS_DEMAND: usize = 5;

pub(crate) fn register_p69_websocket(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // WebSocket.Builder — 3-field: uri=0, headers=1, subprotocol=2
    let wsb = "java/net/http/WebSocket$Builder";
    r.register(
        wsb,
        "header",
        "(Ljava/lang/String;Ljava/lang/String;)Ljava/net/http/WebSocket$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        wsb,
        "connectTimeout",
        "(Ljava/time/Duration;)Ljava/net/http/WebSocket$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    r.register(
        wsb,
        "subprotocols",
        "(Ljava/lang/String;[Ljava/lang/String;)Ljava/net/http/WebSocket$Builder;",
        |_ctx, args| Ok(Some(args.first().copied().unwrap_or(Value::Object(None)))),
    );
    // buildAsync — real HTTP Upgrade + RFC 6455 handshake
    r.register(wsb, "buildAsync", "(Ljava/net/URI;Ljava/net/http/WebSocket$Listener;)Ljava/util/concurrent/CompletableFuture;", |ctx, args| {
        // Extract URI string from the URI object (field 0 is the string form)
        let uri_str = if let Some(Value::Object(Some(uri_obj))) = args.get(1) {
            if let Value::Object(Some(s)) = ctx.get_field(*uri_obj, 0) {
                ctx.read_string(s).unwrap_or_default()
            } else {
                String::new()
            }
        } else {
            String::new()
        };

        // Parse ws:// or wss:// URI
        let (use_tls, host, port, path) = ws_parse_uri(&uri_str);

        // Connect
        let fd_id = if use_tls {
            // `open_tls_connect` has no `_checked` twin on `FileDescriptorTable`,
            // so gate the endpoint explicitly before the connect.
            crate::capability_gate::gate_network(&*ctx, &format!("{}:{}", host, port))?;
            ctx.fd_table().open_tls_connect(&host, port)
                .map_err(|e| RuntimeError::IOException { message: e.to_string() })?
        } else {
            crate::capability_gate::open_tcp_connect_gated(&*ctx, &format!("{}:{}", host, port))
                .map_err(|e| crate::capability_gate::translate_open_failure(e, |io| io.to_string()))?
        };

        // Generate WebSocket key (16 random bytes, base64-encoded)
        let ws_key = ws_generate_key();

        // Send HTTP Upgrade request
        let upgrade_req = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: {}\r\n\r\n",
            path, host, ws_key
        );
        let req_bytes = upgrade_req.as_bytes();
        // STW-TAKEOVER-FIX (2026-07-14): this whole handshake (write +
        // byte-at-a-time read loop below) is genuinely blocking network I/O
        // with no bound on wait time and, until now, no GC-barrier
        // cooperation at all — the JDK `HttpClient`/`WebSocket` "Standard"
        // client path used by e.g.
        // `web.socket.messaging.StompWebSocketIntegrationTests`'s
        // `client = Standard` parameterization. No Java ObjectRef is held
        // across this section (the URI string was already read above; the
        // WebSocket object/CompletableFuture are only allocated after this
        // block completes), so a plain begin/end pair suffices.
        ctx.begin_blocking_region();
        if use_tls {
            let _ = ctx.fd_table().tls_write(fd_id, req_bytes);
        } else {
            let _ = ctx.fd_table().tcp_write(fd_id, req_bytes);
        }

        // Read upgrade response (wait for \r\n\r\n)
        let mut response = Vec::new();
        let mut buf = [0u8; 1];
        for _ in 0..8192 {
            let n = if use_tls {
                ctx.fd_table().tls_read(fd_id, &mut buf).unwrap_or(0)
            } else {
                ctx.fd_table().tcp_read(fd_id, &mut buf).unwrap_or(0)
            };
            if n == 0 { break; }
            response.push(buf[0]);
            if response.len() >= 4 && response[response.len()-4..] == *b"\r\n\r\n" {
                break;
            }
        }
        ctx.end_blocking_region();

        // Verify 101 Switching Protocols
        let resp_str = String::from_utf8_lossy(&response);
        if !resp_str.starts_with("HTTP/1.1 101") {
            let _ = ctx.fd_table().close(fd_id);
            return Err(RuntimeError::IOException {
                message: format!("WebSocket upgrade failed: {}", resp_str.lines().next().unwrap_or("no response")),
            }.into());
        }

        // Verify Sec-WebSocket-Accept
        let expected_accept = ws_compute_accept(&ws_key);
        let accept_ok = resp_str.lines().any(|line| {
            if let Some(val) = line.strip_prefix("Sec-WebSocket-Accept: ") {
                val.trim() == expected_accept
            } else if let Some(val) = line.strip_prefix("sec-websocket-accept: ") {
                val.trim() == expected_accept
            } else {
                false
            }
        });
        if !accept_ok {
            let _ = ctx.fd_table().close(fd_id);
            return Err(RuntimeError::IOException {
                message: "WebSocket Sec-WebSocket-Accept mismatch".into(),
            }.into());
        }

        // Create WebSocket object
        let ws_obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/WebSocket", 6)?;
        ctx.set_field(ws_obj, P69_WS_FD, Value::Int(fd_id as i32));
        ctx.set_field(ws_obj, P69_WS_OUT_CLOSED, Value::Int(0));
        ctx.set_field(ws_obj, P69_WS_IN_CLOSED, Value::Int(0));
        ctx.set_field(ws_obj, P69_WS_DEMAND, Value::Long(0));
        let sp = ctx.create_string("");
        ctx.set_field(ws_obj, P69_WS_SUBPROTO, Value::Object(Some(sp)));
        ctx.set_field(ws_obj, P69_WS_STATE, Value::Int(0)); // open

        // Store whether this is TLS in a way the send methods can use
        // We'll use the sign of fd_id: negative = TLS
        if use_tls {
            ctx.set_field(ws_obj, P69_WS_FD, Value::Int(-(fd_id as i32)));
        }

        let cf = p58_new_cf(ctx, Value::Object(Some(ws_obj)), true)?;
        Ok(Some(Value::Object(Some(cf))))
    });

    // WebSocket methods with real RFC 6455 framing
    let ws = "java/net/http/WebSocket";

    // sendText(CharSequence, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        ws,
        "sendText",
        "(Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let text = match args.get(1) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            let last = args.get(2).and_then(|v| v.as_int()).unwrap_or(1) != 0;
            let opcode = if last { 0x81 } else { 0x01 }; // FIN + text opcode, or continuation
            ws_send_frame(ctx, this, opcode, text.as_bytes())?;
            let cf = p58_new_cf(ctx, Value::Object(Some(this)), true)?;
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendBinary(ByteBuffer, boolean last) -> CompletableFuture<WebSocket>
    r.register(
        ws,
        "sendBinary",
        "(Ljava/nio/ByteBuffer;Z)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Read ByteBuffer data (field 0 = backing array)
            let data = match args.get(1) {
                Some(Value::Object(Some(bb))) => match ctx.get_field(*bb, 0) {
                    Value::Object(Some(arr)) => {
                        let len = ctx.array_length(arr);
                        let mut bytes = Vec::with_capacity(len);
                        for i in 0..len {
                            if let Value::Int(b) = ctx.get_array_element(arr, i) {
                                bytes.push(b as u8);
                            }
                        }
                        bytes
                    }
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            let last = args.get(2).and_then(|v| v.as_int()).unwrap_or(1) != 0;
            let opcode = if last { 0x82 } else { 0x02 }; // FIN + binary opcode
            ws_send_frame(ctx, this, opcode, &data)?;
            let cf = p58_new_cf(ctx, Value::Object(Some(this)), true)?;
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendPing(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        ws,
        "sendPing",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let data = match args.get(1) {
                Some(Value::Object(Some(bb))) => match ctx.get_field(*bb, 0) {
                    Value::Object(Some(arr)) => {
                        let len = std::cmp::min(ctx.array_length(arr), 125); // max 125 bytes for control frames
                        let mut bytes = Vec::with_capacity(len);
                        for i in 0..len {
                            if let Value::Int(b) = ctx.get_array_element(arr, i) {
                                bytes.push(b as u8);
                            }
                        }
                        bytes
                    }
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            ws_send_frame(ctx, this, 0x89, &data)?; // 0x89 = FIN + Ping
            let cf = p58_new_cf(ctx, Value::Object(Some(this)), true)?;
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendPong(ByteBuffer) -> CompletableFuture<WebSocket>
    r.register(
        ws,
        "sendPong",
        "(Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let data = match args.get(1) {
                Some(Value::Object(Some(bb))) => match ctx.get_field(*bb, 0) {
                    Value::Object(Some(arr)) => {
                        let len = std::cmp::min(ctx.array_length(arr), 125);
                        let mut bytes = Vec::with_capacity(len);
                        for i in 0..len {
                            if let Value::Int(b) = ctx.get_array_element(arr, i) {
                                bytes.push(b as u8);
                            }
                        }
                        bytes
                    }
                    _ => Vec::new(),
                },
                _ => Vec::new(),
            };
            ws_send_frame(ctx, this, 0x8A, &data)?; // 0x8A = FIN + Pong
            let cf = p58_new_cf(ctx, Value::Object(Some(this)), true)?;
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // sendClose(int statusCode, String reason) -> CompletableFuture<WebSocket>
    r.register(
        ws,
        "sendClose",
        "(ILjava/lang/String;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let status = args.get(1).and_then(|v| v.as_int()).unwrap_or(1000) as u16;
            let reason = match args.get(2) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            // Close frame payload: 2-byte status code + reason text
            let mut payload = Vec::with_capacity(2 + reason.len());
            payload.extend_from_slice(&status.to_be_bytes());
            payload.extend_from_slice(reason.as_bytes());
            ws_send_frame(ctx, this, 0x88, &payload)?; // 0x88 = FIN + Close
            ctx.set_field(this, P69_WS_OUT_CLOSED, Value::Int(1));
            ctx.set_field(this, P69_WS_STATE, Value::Int(1)); // closing
            let cf = p58_new_cf(ctx, Value::Object(Some(this)), true)?;
            Ok(Some(Value::Object(Some(cf))))
        },
    );

    // WebSocket.request(long n) — add to demand counter for backpressure
    r.register(ws, "request", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let n = match args.get(1) {
            Some(Value::Long(v)) => *v,
            _ => 0,
        };
        if n < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "request count must be non-negative".into(),
            }
            .into());
        }
        if ctx.object_num_fields(this) > P69_WS_DEMAND {
            let current = match ctx.get_field(this, P69_WS_DEMAND) {
                Value::Long(v) => v,
                _ => 0,
            };
            // Saturating add
            let new_demand = current.saturating_add(n);
            ctx.set_field(this, P69_WS_DEMAND, Value::Long(new_demand));
        }
        Ok(None)
    });
    r.register(ws, "getSubprotocol", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, P69_WS_SUBPROTO)))
    });
    r.register(ws, "isOutputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, P69_WS_OUT_CLOSED)))
    });
    r.register(ws, "isInputClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, P69_WS_IN_CLOSED)))
    });
    r.register(ws, "abort", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_val = ctx.get_field(this, P69_WS_FD).as_int().unwrap_or(0);
        let fd_id = fd_val.unsigned_abs();
        if fd_id > 0 {
            let _ = ctx.fd_table().close(fd_id);
        }
        ctx.set_field(this, P69_WS_OUT_CLOSED, Value::Int(1));
        ctx.set_field(this, P69_WS_IN_CLOSED, Value::Int(1));
        ctx.set_field(this, P69_WS_STATE, Value::Int(2)); // closed
        Ok(None)
    });

    // WebSocket.Listener — default methods
    // Per JDK spec, Listener.onOpen default calls webSocket.request(1L) to start receiving.
    let wsl = "java/net/http/WebSocket$Listener";
    r.register(
        wsl,
        "onOpen",
        "(Ljava/net/http/WebSocket;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(ws))) = args.get(1) {
                let _ = ctx.invoke_virtual(*ws, "request", "(J)V", &[Value::Long(1)]);
            }
            Ok(None)
        },
    );
    // onText / onBinary / onPing / onPong: the JDK defaults are
    //     webSocket.request(1); return null;
    // NOT `return null`. Returning null alone leaves the demand counter at the
    // single unit `onOpen` handed out, so a listener that relies on the default
    // for any one message type receives exactly one message and then stalls
    // forever — a backpressure deadlock, not an error. Renew the demand here,
    // through the WebSocket's own `request` native (which saturating-adds to
    // P69_WS_DEMAND), exactly as `onOpen` above does. Errors from the upcall are
    // dropped: the JDK default cannot fail, and a failure to renew demand must
    // not become a second, different failure in the caller's message callback.
    fn ws_listener_default_renew_demand(
        ctx: &mut dyn NativeContext,
        args: &[Value],
    ) -> MethodCallResult {
        if let Some(Value::Object(Some(ws))) = args.get(1) {
            let _ = ctx.invoke_virtual(*ws, "request", "(J)V", &[Value::Long(1)]);
        }
        Ok(Some(Value::Object(None)))
    }
    r.register(
        wsl,
        "onText",
        "(Ljava/net/http/WebSocket;Ljava/lang/CharSequence;Z)Ljava/util/concurrent/CompletionStage;",
        ws_listener_default_renew_demand,
    );
    r.register(
        wsl,
        "onBinary",
        "(Ljava/net/http/WebSocket;Ljava/nio/ByteBuffer;Z)Ljava/util/concurrent/CompletionStage;",
        ws_listener_default_renew_demand,
    );
    r.register(
        wsl,
        "onPing",
        "(Ljava/net/http/WebSocket;Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletionStage;",
        ws_listener_default_renew_demand,
    );
    r.register(
        wsl,
        "onPong",
        "(Ljava/net/http/WebSocket;Ljava/nio/ByteBuffer;)Ljava/util/concurrent/CompletionStage;",
        ws_listener_default_renew_demand,
    );
    // KEEP: `Listener.onClose`'s JDK default really is `return null;` — it is
    // the one message callback that does NOT renew demand (the connection is
    // closing, so there is nothing left to request). The constant is the spec.
    r.register(
        wsl,
        "onClose",
        "(Ljava/net/http/WebSocket;ILjava/lang/String;)Ljava/util/concurrent/CompletionStage;",
        |_ctx, _args| Ok(Some(Value::Object(None))),
    );
    // KEEP: `Listener.onError`'s JDK default body is genuinely empty
    // (`default void onError(WebSocket ws, Throwable error) {}`) — the
    // no-op IS the spec. It cannot swallow a user override either: `handle`
    // resolves to the implementing class, so native lookup (keyed on the
    // resolved method's declaring class) never reaches this interface entry
    // for a class that declares its own `onError`.
    r.register(
        wsl,
        "onError",
        "(Ljava/net/http/WebSocket;Ljava/lang/Throwable;)V",
        native_noop_with_this,
    );

    // HttpClient.newWebSocketBuilder
    r.register(
        "java/net/http/HttpClient",
        "newWebSocketBuilder",
        "()Ljava/net/http/WebSocket$Builder;",
        |ctx, _args| {
            let obj = try_alloc_concurrent_synthetic(ctx, "java/net/http/WebSocket$Builder", 3)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.set_category(__prev_cat);
}

// ===========================================================================
// WebSocket RFC 6455 helper functions
// ===========================================================================

/// Parse a ws:// or wss:// URI into (use_tls, host, port, path)
pub(crate) fn ws_parse_uri(uri: &str) -> (bool, String, u16, String) {
    let (use_tls, rest) = if let Some(r) = uri.strip_prefix("wss://") {
        (true, r)
    } else if let Some(r) = uri.strip_prefix("ws://") {
        (false, r)
    } else {
        (false, uri)
    };
    let default_port = if use_tls { 443 } else { 80 };
    let (host_port, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };
    let (host, port) = if let Some(colon) = host_port.rfind(':') {
        let port_str = &host_port[colon + 1..];
        if let Ok(p) = port_str.parse::<u16>() {
            (host_port[..colon].to_string(), p)
        } else {
            (host_port.to_string(), default_port)
        }
    } else {
        (host_port.to_string(), default_port)
    };
    (use_tls, host, port, path.to_string())
}

/// Generate a random 16-byte WebSocket key, base64-encoded
pub(crate) fn ws_generate_key() -> String {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut rng = seed;
    let mut key_bytes = [0u8; 16];
    for b in key_bytes.iter_mut() {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *b = (rng >> 33) as u8;
    }
    ws_base64_encode(&key_bytes)
}

/// Compute Sec-WebSocket-Accept value: base64(SHA-1(key + GUID))
pub(crate) fn ws_compute_accept(key: &str) -> String {
    use crate::real_sha1;
    let magic = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let mut input = key.to_string();
    input.push_str(magic);
    let hash = real_sha1(input.as_bytes());
    ws_base64_encode(&hash)
}

/// Simple base64 encoder
pub(crate) fn ws_base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    let mut i = 0;
    while i + 2 < data.len() {
        let b0 = data[i] as u32;
        let b1 = data[i + 1] as u32;
        let b2 = data[i + 2] as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        result.push(ALPHABET[(triple & 0x3F) as usize] as char);
        i += 3;
    }
    let remaining = data.len() - i;
    if remaining == 2 {
        let b0 = data[i] as u32;
        let b1 = data[i + 1] as u32;
        let triple = (b0 << 16) | (b1 << 8);
        result.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        result.push('=');
    } else if remaining == 1 {
        let b0 = data[i] as u32;
        let triple = b0 << 16;
        result.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        result.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        result.push('=');
        result.push('=');
    }
    result
}

/// Send a WebSocket frame (RFC 6455 Section 5.2)
/// Client frames MUST be masked.
pub(crate) fn ws_send_frame(
    ctx: &mut dyn NativeContext,
    ws_obj: ObjectRef,
    first_byte: u8,
    payload: &[u8],
) -> Result<(), MethodCallFailed> {
    let fd_val = ctx.get_field(ws_obj, P69_WS_FD).as_int().unwrap_or(0);
    let use_tls = fd_val < 0;
    let fd_id = fd_val.unsigned_abs();

    if fd_id == 0 {
        return Err(RuntimeError::IOException {
            message: "WebSocket not connected".into(),
        }
        .into());
    }

    let mut frame = Vec::with_capacity(14 + payload.len());
    frame.push(first_byte);

    // Payload length + mask bit (0x80 = masked)
    let len = payload.len();
    if len < 126 {
        frame.push(0x80 | len as u8);
    } else if len < 65536 {
        frame.push(0x80 | 126);
        frame.push((len >> 8) as u8);
        frame.push(len as u8);
    } else {
        frame.push(0x80 | 127);
        let len64 = len as u64;
        frame.extend_from_slice(&len64.to_be_bytes());
    }

    // Masking key (4 random bytes)
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let mut rng = seed;
    let mut mask = [0u8; 4];
    for m in mask.iter_mut() {
        rng = rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *m = (rng >> 33) as u8;
    }
    frame.extend_from_slice(&mask);

    // Masked payload
    for (i, &b) in payload.iter().enumerate() {
        frame.push(b ^ mask[i % 4]);
    }

    // Write frame
    let write_result = if use_tls {
        ctx.fd_table().tls_write(fd_id, &frame)
    } else {
        ctx.fd_table().tcp_write(fd_id, &frame)
    };
    write_result.map_err(|e| RuntimeError::IOException {
        message: e.to_string(),
    })?;
    Ok(())
}

// =============================================================================
// java.net UDP — DatagramPacket, DatagramSocket, MulticastSocket
// =============================================================================

/// `DatagramPacket` slot 4 — the buffer offset. Guarded at every use: see
/// `getOffset`'s note on the missing `synthetic_stub_fields` entry.
const DP_OFFSET: usize = 4;

pub(crate) fn register_p72_datagram(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // DatagramPacket = 5-field (data=0, length=1, address=2, port=3, offset=4)
    let dp = "java/net/DatagramPacket";
    r.register(dp, "<init>", "([BI)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        let len = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(len));
        ctx.set_field(this, 2, Value::Object(None));
        ctx.set_field(this, 3, Value::Int(0));
        Ok(None)
    });
    r.register(
        dp,
        "<init>",
        "([BILjava/net/InetAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            let len = match args.get(2) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            ctx.set_field(this, 1, Value::Int(len));
            ctx.set_field(this, 2, args.get(3).copied().unwrap_or(Value::Object(None)));
            let port = match args.get(4) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(port));
            Ok(None)
        },
    );
    r.register(
        dp,
        "<init>",
        "([BIILjava/net/InetAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
            let len = match args.get(3) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            ctx.set_field(this, 1, Value::Int(len));
            ctx.set_field(this, 2, args.get(4).copied().unwrap_or(Value::Object(None)));
            let port = match args.get(5) {
                Some(Value::Int(i)) => *i,
                _ => 0,
            };
            ctx.set_field(this, 3, Value::Int(port));
            // Slot 4 = offset (the argument this ctor used to drop). Guarded:
            // see `getOffset` below — the synthetic layout does not always have
            // the slot, and a packet built without it keeps today's offset-0
            // behaviour rather than writing out of bounds.
            if ctx.object_num_fields(this) > DP_OFFSET {
                let offset = match args.get(2) {
                    Some(Value::Int(i)) => *i,
                    _ => 0,
                };
                ctx.set_field(this, DP_OFFSET, Value::Int(offset));
            }
            Ok(None)
        },
    );
    r.register(dp, "getData", "()[B", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 0)))
    });
    r.register(dp, "getLength", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 1)))
    });
    r.register(dp, "setLength", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, args.get(1).copied().unwrap_or(Value::Int(0)));
        Ok(None)
    });
    r.register(dp, "getAddress", "()Ljava/net/InetAddress;", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 2)))
    });
    r.register(dp, "getPort", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 3)))
    });
    r.register(dp, "setData", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        Ok(None)
    });
    r.register(
        dp,
        "setAddress",
        "(Ljava/net/InetAddress;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 2, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        },
    );
    r.register(dp, "setPort", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 3, args.get(1).copied().unwrap_or(Value::Int(0)));
        Ok(None)
    });
    // Reads the offset the 5-arg constructor now stores in slot 4, falling
    // back to 0 for a packet whose layout has no such slot (the other two
    // constructors have no offset argument, and 0 is their correct answer).
    //
    // CLOSED — the cross-crate change this paragraph asked for HAS landed:
    // `classloading/src/class_manager.rs` now carries
    // `"java/net/DatagramPacket" => instance_fields(5)`, and
    // `t9c_synthetic_field_tables_cover_their_factories` asserts the 5. So the
    // guard passes (5 > DP_OFFSET) and the slot-4 write is LIVE, not inert.
    // The guard stays because this registrar is reachable only in a
    // `--synthetic-jdk` build (`register_phase72_natives` is called from
    // lib.rs's `#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`)
    // and a shorter carrier would still silently drop the write.
    r.register(dp, "getOffset", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) > DP_OFFSET {
            // `unwrap_or(0)` also covers a packet built by one of the two
            // offset-less constructors, which leave the slot uninitialised.
            let off = ctx.get_field(this, DP_OFFSET).as_int().unwrap_or(0);
            return Ok(Some(Value::Int(off)));
        }
        Ok(Some(Value::Int(0)))
    });

    // DatagramSocket = 4-field (port=0, closed=1, timeout=2, fd_id=3)
    // The `java/net/DatagramSocket` set that used to live here is DELETED.
    // `net_phase_e::register_re7_datagram_socket` owns the class: it keeps its
    // state in `ds_side_table` and drives a real UDP fd, while this set wrote
    // raw slots on the same objects. This file registers LATER, so in a
    // `--synthetic-jdk` build the slot-based set won every overlapping key and
    // the working implementation was shadowed; and because this registrar is
    // `#[cfg(feature = "synthetic-jdk")]`, the four keys it owned ALONE
    // (`isBound`, `getLocalAddress`, `setBroadcast`, `getBroadcast`) did not
    // exist at all in the default build. All four moved to RE7, so one owner
    // now covers the whole class in both builds. `DatagramPacket` and
    // `DatagramChannel` below are unaffected — RE7 does not register those.

    // send(DatagramPacket) — real UDP send via fd_table

    // receive(DatagramPacket) — real UDP receive via fd_table

    // setReuseAddress / getReuseAddress were a discard-then-lie pair: the setter
    // threw the flag away and the getter always answered `false`, so
    // `s.setReuseAddress(true); s.getReuseAddress()` returned false. Both halves
    // now go to the real socket — `fd_table` already exposes the setter, and the
    // getter reads SO_REUSEADDR back through socket2 on a dup of the fd (dup'ing
    // shares the option state; dropping the dup closes only the duplicate).

    // The escalated primitive landed: `FdTable::udp_disconnect(FdId)`
    // (native-api/src/fd_table.rs) issues the POSIX `connect(AF_UNSPEC)` that
    // dissolves the association, treating the `EAFNOSUPPORT`/`WSAEAFNOSUPPORT`
    // both platforms report for that form as success. So this is no longer a
    // no-op: the fd is resolved exactly the way the sibling `connect` above
    // resolves it, and the kernel-side association is really undone. Without
    // it the socket stayed connected — still dropping datagrams from every
    // peer but the old one — while the Java object claimed to be disconnected.
    //
    // There is NO Java-side `connected` state to clear here: this class's
    // layout is port/closed/timeout/fd_id and has no such flag, `connect`
    // above sets none, and no `isConnected`/`getInetAddress`/`getPort` native
    // is registered for `java/net/DatagramSocket` to read one. The fd IS the
    // state, so the two halves agree once the fd is disconnected.
    //
    // `DatagramSocket.disconnect()` is specified never to throw and to be a
    // no-op on a socket that was never connected (the AF_UNSPEC form is itself
    // a no-op on an unassociated socket), so the result is deliberately
    // swallowed.
    //
    // CAVEAT — read before trusting this at runtime: slot 3 is very probably
    // out of bounds for a real `new java.net.DatagramSocket()`. There is no
    // `"java/net/DatagramSocket"` arm in `classloading/src/class_manager.rs`
    // `synthetic_stub_fields`, so a synthetic-mode instance falls to that
    // function's `_ => vec![]` arm and gets ZERO slots, and the real JDK 17+
    // class has a single `delegate` field (which is the documented reason
    // `net_phase_e::register_re7_datagram_socket` keeps its state in a
    // `DsSide` side table keyed by ObjectRef instead of in slots). That makes
    // the whole 4-slot `ds` set here — `<init>`, `send`, `close`, `connect`
    // and now `disconnect` — inert in the same silent way wave 4 found for
    // `DatagramPacket.getOffset`: `set_field` past the end DROPS the write.
    // Fixing that is a cross-crate change (add
    // `"java/net/DatagramSocket" => instance_fields(4)` there, or better, move
    // this set onto the RE.7 side table); the call below is correct the moment
    // the fd is actually stored, and no worse than the old no-op until then.

    // `setBroadcast` above really does set SO_BROADCAST on the fd, so a constant
    // `false` getter contradicted the setter that had just run. Read the option
    // back off the socket (via a dup, which shares option state).

    // MulticastSocket = 5-field (port=0, closed=1, timeout=2, fd_id=3, ttl=4)
    let ms = "java/net/MulticastSocket";
    r.register(ms, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id =
            crate::capability_gate::open_udp_gated(&*ctx, Some("0.0.0.0:0")).map_err(|e| {
                crate::capability_gate::translate_open_failure(e, |io| {
                    format!("MulticastSocket bind failed: {io}")
                })
            })?;
        ctx.set_field(this, 0, Value::Int(0));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(0));
        ctx.set_field(this, 3, Value::Int(fd_id as i32));
        ctx.set_field(this, 4, Value::Int(1)); // default TTL = 1
        Ok(None)
    });
    r.register(ms, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = match args.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let bind_addr = format!("0.0.0.0:{}", port);
        let fd_id =
            crate::capability_gate::open_udp_gated(&*ctx, Some(&bind_addr)).map_err(|e| {
                crate::capability_gate::translate_open_failure(e, |io| {
                    format!("MulticastSocket bind failed: {io}")
                })
            })?;
        ctx.set_field(this, 0, Value::Int(port));
        ctx.set_field(this, 1, Value::Int(0));
        ctx.set_field(this, 2, Value::Int(0));
        ctx.set_field(this, 3, Value::Int(fd_id as i32));
        ctx.set_field(this, 4, Value::Int(1));
        Ok(None)
    });
    r.register(ms, "joinGroup", "(Ljava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd_id < 0 {
            return Err(RuntimeError::IOException {
                message: "Socket closed".into(),
            }
            .into());
        }
        let addr_ref = obj_arg(args, 1)?;
        let addr_str = match crate::net_phase_e::inet_addr_get(addr_ref) {
            Some((_, ip)) if !ip.is_empty() => ip,
            _ => match ctx.get_field(addr_ref, 1) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => {
                    return Err(RuntimeError::IOException {
                        message: "Invalid multicast address".into(),
                    }
                    .into())
                }
            },
        };
        let mcast_ip: std::net::Ipv4Addr =
            addr_str.parse().map_err(|_| RuntimeError::IOException {
                message: format!("Invalid multicast address: {}", addr_str),
            })?;
        ctx.fd_table()
            .udp_join_multicast_v4(fd_id as u32, &mcast_ip, &std::net::Ipv4Addr::UNSPECIFIED)
            .map_err(|e| RuntimeError::IOException {
                message: format!("joinGroup failed: {}", e),
            })?;
        Ok(None)
    });
    r.register(
        ms,
        "leaveGroup",
        "(Ljava/net/InetAddress;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
            if fd_id < 0 {
                return Err(RuntimeError::IOException {
                    message: "Socket closed".into(),
                }
                .into());
            }
            let addr_ref = obj_arg(args, 1)?;
            let addr_str = match crate::net_phase_e::inet_addr_get(addr_ref) {
                Some((_, ip)) if !ip.is_empty() => ip,
                _ => match ctx.get_field(addr_ref, 1) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => {
                        return Err(RuntimeError::IOException {
                            message: "Invalid multicast address".into(),
                        }
                        .into())
                    }
                },
            };
            let mcast_ip: std::net::Ipv4Addr =
                addr_str.parse().map_err(|_| RuntimeError::IOException {
                    message: format!("Invalid multicast address: {}", addr_str),
                })?;
            ctx.fd_table()
                .udp_leave_multicast_v4(fd_id as u32, &mcast_ip, &std::net::Ipv4Addr::UNSPECIFIED)
                .map_err(|e| RuntimeError::IOException {
                    message: format!("leaveGroup failed: {}", e),
                })?;
            Ok(None)
        },
    );
    r.register(ms, "setTimeToLive", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ttl = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().udp_set_ttl(fd_id as u32, ttl as u32);
        }
        ctx.set_field(this, 4, Value::Int(ttl));
        Ok(None)
    });
    r.register(ms, "getTimeToLive", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    });
    r.register(ms, "send", "(Ljava/net/DatagramPacket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd_id < 0 || ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
            return Err(RuntimeError::IOException {
                message: "Socket closed".into(),
            }
            .into());
        }
        let pkt = obj_arg(args, 1)?;
        let data_arr = match ctx.get_field(pkt, 0) {
            Value::Object(Some(a)) => a,
            _ => {
                return Err(RuntimeError::IOException {
                    message: "No packet data".into(),
                }
                .into())
            }
        };
        let length = ctx.get_field(pkt, 1).as_int().unwrap_or(0) as usize;
        let offset = ctx.get_field(pkt, 2).as_int().unwrap_or(0) as usize;
        let mut buf = vec![0u8; length];
        for i in 0..length {
            buf[i] = ctx
                .get_array_element(data_arr, offset + i)
                .as_int()
                .unwrap_or(0) as u8;
        }
        // Extract target address from packet
        let addr_str = match ctx.get_field(pkt, 3) {
            Value::Object(Some(addr)) => {
                let host = match ctx.get_field(addr, 1) {
                    Value::Object(Some(s)) => {
                        ctx.read_string(s).unwrap_or_else(|| "127.0.0.1".into())
                    }
                    _ => "127.0.0.1".into(),
                };
                host
            }
            _ => "127.0.0.1".into(),
        };
        let port = ctx.get_field(pkt, 4).as_int().unwrap_or(0);
        let target = format!("{}:{}", addr_str, port);
        ctx.fd_table()
            .udp_send(fd_id as u32, &buf, &target)
            .map_err(|e| RuntimeError::IOException {
                message: format!("send failed: {}", e),
            })?;
        Ok(None)
    });
    r.register(
        ms,
        "receive",
        "(Ljava/net/DatagramPacket;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
            if fd_id < 0 || ctx.get_field(this, 1).as_int().unwrap_or(0) != 0 {
                return Err(RuntimeError::IOException {
                    message: "Socket closed".into(),
                }
                .into());
            }
            let timeout = ctx.get_field(this, 2).as_int().unwrap_or(0);
            if timeout > 0 {
                let _ = ctx.fd_table().udp_set_read_timeout(
                    fd_id as u32,
                    Some(std::time::Duration::from_millis(timeout as u64)),
                );
            }
            let pkt = obj_arg(args, 1)?;
            let data_arr = match ctx.get_field(pkt, 0) {
                Value::Object(Some(a)) => a,
                _ => {
                    return Err(RuntimeError::IOException {
                        message: "No packet buffer".into(),
                    }
                    .into())
                }
            };
            let buf_len = ctx.array_length(data_arr);
            let mut buf = vec![0u8; buf_len];
            let (n, src_str) = ctx
                .fd_table()
                .udp_recv(fd_id as u32, &mut buf)
                .map_err(|e| RuntimeError::IOException {
                    message: format!("receive failed: {}", e),
                })?;
            for i in 0..n {
                ctx.set_array_element(data_arr, i, Value::Int(buf[i] as i8 as i32));
            }
            ctx.set_field(pkt, 1, Value::Int(n as i32)); // length
            ctx.set_field(pkt, 2, Value::Int(0)); // offset
                                                  // Parse source address "ip:port"
            let (src_ip_str, src_port) = if let Some(colon) = src_str.rfind(':') {
                (
                    &src_str[..colon],
                    src_str[colon + 1..].parse::<i32>().unwrap_or(0),
                )
            } else {
                (src_str.as_str(), 0)
            };
            let src_addr = crate::net_phase_e::alloc_inet_address_unnamed(ctx, src_ip_str)?;
            ctx.set_field(pkt, 3, Value::Object(Some(src_addr)));
            ctx.set_field(pkt, 4, Value::Int(src_port));
            Ok(None)
        },
    );
    r.register(ms, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            let _ = ctx.fd_table().close(fd_id as u32);
        }
        ctx.set_field(this, 1, Value::Int(1));
        ctx.set_field(this, 3, Value::Int(-1));
        Ok(None)
    });
    r.register(ms, "isClosed", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 1)))
    });
    r.register(ms, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(timeout));
        Ok(None)
    });
    r.register(ms, "setReuseAddress", "(Z)V", |ctx, args| {
        // MulticastSocket.setReuseAddress — apply SO_REUSEADDR via socket2 on the underlying UdpSocket.
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            ctx.fd_table().udp_set_reuse_address(fd_id as u32, on).ok();
        }
        Ok(None)
    });
    r.register(ms, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// java.nio.channels.DatagramChannel — non-blocking UDP (JEP U6)
// =============================================================================

/// Read a `SocketOption`'s `name()` (`"SO_BROADCAST"`, `"SO_RCVBUF"`, …).
///
/// Real `StandardSocketOptions` members expose the name through a `name` field;
/// the synthetic ones answer the `name()` accessor. Try the field first (no
/// upcall, so no allocation and no safepoint) and fall back to the virtual call.
fn dc_option_name(ctx: &mut dyn NativeContext, opt: Option<Value>) -> String {
    let obj = match opt {
        Some(Value::Object(Some(o))) => o,
        _ => return String::new(),
    };
    if let Value::Object(Some(s)) = ctx.get_field_by_name(obj, "name") {
        if let Some(name) = ctx.read_string(s) {
            if !name.is_empty() {
                return name;
            }
        }
    }
    match ctx.invoke_virtual(obj, "name", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Unwrap a `SocketOption` payload, which arrives boxed (`Boolean`/`Integer`)
/// because the setter is erased to `(SocketOption, Object)`.
fn dc_option_int(ctx: &mut dyn NativeContext, value: Option<Value>) -> i32 {
    match value {
        Some(Value::Int(v)) => v,
        Some(Value::Object(Some(b))) => {
            // Both wrappers keep their scalar in slot 0 (real JDK `value`),
            // so read it directly instead of paying for an upcall.
            match ctx.get_field(b, 0).as_int() {
                Some(v) => v,
                None => match ctx.invoke_virtual(b, "intValue", "()I", &[]) {
                    Ok(Some(Value::Int(v))) => v,
                    _ => 0,
                },
            }
        }
        _ => 0,
    }
}

/// Box a socket-option value as the type its `SocketOption<T>` declares.
/// `getOption` is `<T> T`, so returning a raw `Value::Int` for an object-typed
/// method coerces to null and the caller NPEs on the unbox.
fn dc_box_option(ctx: &mut dyn NativeContext, name: &str, raw: i32) -> MethodCallResult {
    if matches!(name, "SO_BROADCAST" | "SO_REUSEADDR" | "SO_REUSEPORT") {
        ctx.invoke(
            "java/lang/Boolean",
            "valueOf",
            "(Z)Ljava/lang/Boolean;",
            &[Value::Int(if raw != 0 { 1 } else { 0 })],
        )
    } else {
        ctx.invoke(
            "java/lang/Integer",
            "valueOf",
            "(I)Ljava/lang/Integer;",
            &[Value::Int(raw)],
        )
    }
}

/// Build a real `java.net.InetSocketAddress` (3-slot object over the JDK's
/// `InetSocketAddressHolder`) for `ip:port`.
///
/// Every intermediate reference is pinned across the allocation that follows
/// it — four allocations happen here and a moving young GC at any of them
/// relocates the ones already built (native stale-local family).
fn p72_alloc_inet_socket_address(
    ctx: &mut dyn NativeContext,
    ip: &str,
    port: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let host0 = ctx.create_string(ip);
    let host_pin = ctx.pin_native_root(host0);
    let addr0 = crate::net_phase_e::alloc_inet_address_unnamed(ctx, ip)?;
    let addr_pin = ctx.pin_native_root(addr0);
    let holder0 = try_alloc_concurrent_synthetic(
        ctx,
        "java/net/InetSocketAddress$InetSocketAddressHolder",
        3,
    )?;
    let holder_pin = ctx.pin_native_root(holder0);
    let isa = try_alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 3)?;
    let host = ctx.read_native_pin(host_pin, host0);
    let addr = ctx.read_native_pin(addr_pin, addr0);
    let holder = ctx.read_native_pin(holder_pin, holder0);
    ctx.set_field(holder, 0, Value::Object(Some(host)));
    ctx.set_field(holder, 1, Value::Object(Some(addr)));
    ctx.set_field(holder, 2, Value::Int(port));
    ctx.set_field(isa, 0, Value::Object(Some(holder)));
    ctx.set_field(isa, 1, Value::Int(port));
    ctx.set_field(isa, 2, Value::Object(Some(addr)));
    ctx.unpin_native_roots(host_pin);
    Ok(isa)
}

pub(crate) fn register_datagram_channel(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    use crate::servlet::{s2_alloc_dgram, s2_register_channel, SocketRegistry};
    let dc = "java/nio/channels/DatagramChannel";

    // DatagramChannel field layout (5 fields, NEW-3):
    //   0: port            (int — local bound port, 0 until bind)
    //   1: open            (int boolean — 1 = open, 0 = closed)
    //   2: connected       (int boolean — 1 = connected to a peer)
    //   3: blocking        (int boolean — 1 = blocking, 0 = non-blocking)
    //   4: sock_id         (int — index into SocketRegistry::dgrams, -1 until bind)
    //
    // The sock_id was added by NEW-3 so that send/receive/read/write
    // share one persistent UDP socket and the Selector can poll its fd.
    //
    // W7-9 — DO NOT WIRE THIS REGISTRAR INTO THE DEFAULT BUILD AS-IS.
    //
    // It is `pub(crate)` and reached only from `phases_late.rs`'s
    // `register_phase72_natives` -> `register_synthetic_overrides`, which is
    // `#[cfg(feature = "synthetic-jdk")]`-gated, so none of it runs in the
    // real-JDK CLI. Two of its bodies — `setOption(SocketOption,Object)` and
    // `getRemoteAddress()` — are `abstract` on the real JDK 25
    // `DatagramChannel` (`javap -p`) and are registered by NOTHING in the
    // default build, so a call on a minted receiver is a hard
    // `AbstractMethodError` there. That reads as "wire this in and the gap
    // closes". It is a trap.
    //
    // `native-io/src/lib.rs` has its OWN `register_datagram_channel` — same
    // class name, live in the default build (twice: directly from
    // `register_io_natives`, and again through
    // `register_phase92_io_completeness`) — and it mints the receiver with a
    // DIFFERENT slot assignment. The bodies below read `field 2` as the
    // connected flag and `field 4` as a `SocketRegistry::dgrams` index. Run
    // them against the object the default build actually allocates and those
    // reads land on unrelated slots: an `Int` where the caller stored a
    // reference, an arbitrary int used as a registry key. That is the
    // slot-index species
    // (docs/known-issues/jdk-only/W4-4-slot-index-species-sweep.md), i.e.
    // heap corruption rather than a wrong answer.
    //
    // The precondition for wiring is therefore NOT "call it later than
    // `register_io_natives`" — it is "one layout owns this class name". Unify
    // first, then move the two bodies into `native-io`'s live registrar, where
    // they need no new `vm_init.rs` call at all. Full adjudication — including
    // the other two absent abstracts, the vectored
    // `read`/`write([Ljava/nio/ByteBuffer;II)J`, which are NOT composable from
    // the single-buffer natives because one call must move exactly one
    // datagram — is in
    // docs/known-issues/jdk-only/W7-9-minted-interface-abstract-methods.md.

    r.register(
        dc,
        "open",
        "()Ljava/nio/channels/DatagramChannel;",
        |ctx, _args| {
            let ch = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/DatagramChannel", 5)?;
            ctx.set_field(ch, 0, Value::Int(0)); // port
            ctx.set_field(ch, 1, Value::Int(1)); // open
            ctx.set_field(ch, 2, Value::Int(0)); // not connected
            ctx.set_field(ch, 3, Value::Int(1)); // blocking mode (default)
            ctx.set_field(ch, 4, Value::Int(-1)); // sock_id (not yet bound)
            Ok(Some(Value::Object(Some(ch))))
        },
    );

    // Helper: lazily bind a 0.0.0.0:<port> UdpSocket for this channel if
    // no persistent socket has been allocated yet. Returns the sock_id.
    fn ensure_bound(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
        host: &str,
        port: i32,
    ) -> Option<i32> {
        let existing = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if existing >= 0 {
            return Some(existing);
        }
        let addr = format!("{host}:{port}");
        let socket = match std::net::UdpSocket::bind(&addr) {
            Ok(s) => s,
            Err(_) => return None,
        };
        // Honor the current blocking mode — the first send/receive before
        // an explicit configureBlocking() defaults to blocking.
        let blocking = ctx.get_field(this, 3).as_int().unwrap_or(1);
        let _ = socket.set_nonblocking(blocking == 0);
        // Record the OS-assigned port if the caller bound to 0.
        if let Ok(local) = socket.local_addr() {
            ctx.set_field(this, 0, Value::Int(local.port() as i32));
        }
        let id = s2_alloc_dgram(socket);
        ctx.set_field(this, 4, Value::Int(id));
        Some(id)
    }

    /// POSIX `connect(AF_UNSPEC)` — dissolve a UDP socket's peer association.
    ///
    /// `FdTable::udp_disconnect` (native-api/src/fd_table.rs) does exactly
    /// this, but it is keyed by `FdId` and a `DatagramChannel`'s socket is NOT
    /// in the fd table: `ensure_bound` above binds a bare
    /// `std::net::UdpSocket` into `servlet::SocketRegistry::dgrams` and slot 4
    /// holds that registry id, not an fd. There is therefore no `FdId` to hand
    /// the primitive, and moving the channel onto the fd table would drag
    /// send/receive/read/write and the Selector's `poll` registration with it.
    /// The syscall is issued against the registry socket instead — the same
    /// way `setOption`/`getOption` below already reach under it through
    /// `socket2::SockRef`.
    ///
    /// Neither `std::net::UdpSocket` nor `socket2` exposes the AF_UNSPEC form,
    /// so it goes through the raw handle. Both platforms report an error for
    /// it even when it works (Linux `EAFNOSUPPORT`, Winsock
    /// `WSAEAFNOSUPPORT`); the disassociation still happens, so that one code
    /// is treated as success. Kept byte-for-byte in step with
    /// `FdTable::udp_disconnect` — if one changes, change both.
    fn udp_dissolve_association(sock: &std::net::UdpSocket) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let mut addr: libc::sockaddr = unsafe { std::mem::zeroed() };
            addr.sa_family = libc::AF_UNSPEC as libc::sa_family_t;
            let rc = unsafe {
                libc::connect(
                    sock.as_raw_fd(),
                    &addr as *const libc::sockaddr,
                    std::mem::size_of::<libc::sockaddr>() as libc::socklen_t,
                )
            };
            if rc == 0 {
                return Ok(());
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EAFNOSUPPORT) {
                return Ok(());
            }
            Err(err)
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawSocket;
            const WSAEAFNOSUPPORT: i32 = 10047;

            // Declared locally, matching the `#[link(name = "ws2_32")]` block
            // in `FdTable::udp_disconnect`. `clashing_extern_declarations` is
            // denied workspace-wide, so these signatures must not diverge from
            // any other declaration of the same symbol in THIS crate — there
            // is currently no other `connect`/`WSAGetLastError` in
            // native-builtins, and this pair matches native-api's.
            #[link(name = "ws2_32")]
            unsafe extern "system" {
                fn connect(s: usize, name: *const u8, namelen: i32) -> i32;
                fn WSAGetLastError() -> i32;
            }

            // `sockaddr` is 16 bytes; all-zero gives sa_family = AF_UNSPEC (0).
            let addr = [0u8; 16];
            let rc = unsafe { connect(sock.as_raw_socket() as usize, addr.as_ptr(), 16) };
            if rc == 0 {
                return Ok(());
            }
            let code = unsafe { WSAGetLastError() };
            if code == WSAEAFNOSUPPORT {
                return Ok(());
            }
            Err(std::io::Error::from_raw_os_error(code))
        }
    }

    r.register(
        dc,
        "bind",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (host, port) = match args.get(1) {
                Some(Value::Object(Some(sa))) => {
                    let h = match ctx.get_field(*sa, 0) {
                        Value::Object(Some(h)) => {
                            ctx.read_string(h).unwrap_or_else(|| "0.0.0.0".into())
                        }
                        _ => "0.0.0.0".into(),
                    };
                    let p = ctx.get_field(*sa, 1).as_int().unwrap_or(0);
                    (h, p)
                }
                _ => ("0.0.0.0".into(), 0),
            };
            // If a previous send/receive auto-bound this channel, rebind by
            // dropping the old socket first so the user-requested address
            // wins. This is the correct JDK semantics for bind after send.
            {
                let mut reg = crate::servlet::s2_registry().lock();
                let old = ctx.get_field(this, 4).as_int().unwrap_or(-1);
                if old >= 0 {
                    reg.dgrams.remove(&old);
                    ctx.set_field(this, 4, Value::Int(-1));
                }
            }
            match ensure_bound(ctx, this, &host, port) {
                Some(_) => Ok(Some(Value::Object(Some(this)))),
                None => Err(RuntimeError::IOException {
                    message: format!("DatagramChannel.bind({host}:{port}) failed"),
                }
                .into()),
            }
        },
    );

    r.register(
        dc,
        "connect",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/DatagramChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (host, port) = match args.get(1) {
                Some(Value::Object(Some(sa))) => {
                    let h = match ctx.get_field(*sa, 0) {
                        Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_default(),
                        _ => String::new(),
                    };
                    let p = ctx.get_field(*sa, 1).as_int().unwrap_or(0);
                    (h, p)
                }
                _ => (String::new(), 0),
            };
            // Auto-bind if not yet bound.
            if ensure_bound(ctx, this, "0.0.0.0", 0).is_none() {
                return Err(RuntimeError::IOException {
                    message: "DatagramChannel.connect: auto-bind failed".into(),
                }
                .into());
            }
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid < 0 {
                return Err(RuntimeError::IOException {
                    message: "DatagramChannel.connect: socket missing".into(),
                }
                .into());
            }
            let target = format!("{host}:{port}");
            {
                let reg = crate::servlet::s2_registry().lock();
                if let Some(sock) = reg.dgrams.get(&sid) {
                    if sock.connect(&target).is_err() {
                        return Err(RuntimeError::IOException {
                            message: format!("DatagramChannel.connect({target}) failed"),
                        }
                        .into());
                    }
                }
            }
            ctx.set_field(this, 2, Value::Int(1));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    r.register(
        dc,
        "disconnect",
        "()Ljava/nio/channels/DatagramChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Was: clear slot 2 and leave the socket alone, with a comment
            // calling that "a portable proxy". It was not a proxy for
            // anything — `connect` above really does associate the socket, so
            // the kernel kept filtering datagrams to the old peer while
            // `isConnected()` answered false and `receive` was documented to
            // accept from anyone again. `udp_dissolve_association` (defined
            // next to `ensure_bound` above) now issues the AF_UNSPEC connect
            // that std::net does not expose. Note it takes the registry socket
            // directly, NOT an `FdId`: `FdTable::udp_disconnect` cannot be
            // used here because this channel's socket lives in
            // `servlet::SocketRegistry::dgrams` and never enters the fd table.
            if ctx.get_field(this, 2).as_int().unwrap_or(0) == 0 {
                // Never connected — the JDK specifies disconnect() as a no-op
                // in that case, and the AF_UNSPEC form is pointless anyway.
                return Ok(Some(Value::Object(Some(this))));
            }
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid >= 0 {
                // Scoped so the registry lock is released before the field
                // write below; nothing inside blocks.
                let outcome = {
                    let reg = crate::servlet::s2_registry().lock();
                    match reg.dgrams.get(&sid) {
                        Some(sock) => udp_dissolve_association(sock),
                        // Connected flag set but the socket is gone: nothing
                        // left to disassociate, so fall through and clear it.
                        None => Ok(()),
                    }
                };
                if let Err(e) = outcome {
                    // `DatagramChannel.disconnect()` is declared `throws
                    // IOException`, and the flag deliberately stays SET: the
                    // peer association is still in place, so answering
                    // "disconnected" would recreate the exact Java/kernel
                    // disagreement this change removes.
                    return Err(RuntimeError::IOException {
                        message: format!("DatagramChannel.disconnect failed: {e}"),
                    }
                    .into());
                }
            }
            ctx.set_field(this, 2, Value::Int(0));
            Ok(Some(Value::Object(Some(this))))
        },
    );

    r.register(dc, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    });

    r.register(dc, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    });

    r.register(dc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 1, Value::Int(0));
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if sid >= 0 {
            crate::servlet::s2_registry().lock().dgrams.remove(&sid);
            ctx.set_field(this, 4, Value::Int(-1));
        }
        Ok(None)
    });

    r.register(
        dc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            ctx.set_field(this, 3, Value::Int(blocking));
            // Apply the mode to the underlying socket if one exists.
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid >= 0 {
                let reg = crate::servlet::s2_registry().lock();
                if let Some(sock) = reg.dgrams.get(&sid) {
                    let _ = sock.set_nonblocking(blocking == 0);
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );

    r.register(
        dc,
        "send",
        "(Ljava/nio/ByteBuffer;Ljava/net/SocketAddress;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Auto-bind to 0.0.0.0:0 on first send — matches JDK behavior.
            if ensure_bound(ctx, this, "0.0.0.0", 0).is_none() {
                return Err(RuntimeError::IOException {
                    message: "DatagramChannel.send: auto-bind failed".into(),
                }
                .into());
            }
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid < 0 {
                return Ok(Some(Value::Int(0)));
            }

            // Destination from SocketAddress.
            let (dest_host, dest_port) = match args.get(2) {
                Some(Value::Object(Some(sa))) => {
                    let h = match ctx.get_field(*sa, 0) {
                        Value::Object(Some(h)) => {
                            ctx.read_string(h).unwrap_or_else(|| "127.0.0.1".into())
                        }
                        _ => "127.0.0.1".into(),
                    };
                    let p = ctx.get_field(*sa, 1).as_int().unwrap_or(0);
                    (h, p)
                }
                _ => ("127.0.0.1".into(), 0),
            };

            // Read the ByteBuffer's remaining bytes.
            let bb = match args.get(1) {
                Some(Value::Object(Some(bb))) => *bb,
                _ => return Ok(Some(Value::Int(0))),
            };
            let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
            let limit = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
            let remaining = limit.saturating_sub(pos);
            if remaining == 0 {
                return Ok(Some(Value::Int(0)));
            }
            let arr = match ctx.get_field(bb, 0) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Int(0))),
            };
            let mut buf = vec![0u8; remaining];
            for i in 0..remaining {
                if let Value::Int(b) = ctx.get_array_element(arr, pos + i) {
                    buf[i] = b as u8;
                }
            }

            let dest = format!("{dest_host}:{dest_port}");
            let sent = {
                let reg = crate::servlet::s2_registry().lock();
                match reg.dgrams.get(&sid) {
                    Some(sock) => match sock.send_to(&buf, &dest) {
                        Ok(n) => n as i32,
                        Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                        Err(_) => -1,
                    },
                    None => -1,
                }
            };
            if sent < 0 {
                return Err(RuntimeError::IOException {
                    message: format!("DatagramChannel.send({dest}) failed"),
                }
                .into());
            }
            ctx.set_field(bb, 1, Value::Int((pos + sent as usize) as i32));
            Ok(Some(Value::Int(sent)))
        },
    );

    r.register(
        dc,
        "receive",
        "(Ljava/nio/ByteBuffer;)Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if ensure_bound(ctx, this, "0.0.0.0", 0).is_none() {
                return Err(RuntimeError::IOException {
                    message: "DatagramChannel.receive: auto-bind failed".into(),
                }
                .into());
            }
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid < 0 {
                return Ok(Some(Value::Object(None)));
            }
            let bb = match args.get(1) {
                Some(Value::Object(Some(bb))) => *bb,
                _ => return Ok(Some(Value::Object(None))),
            };
            // Receive into a scratch buffer sized to the ByteBuffer's
            // remaining bytes. Datagram receive truncates if the buffer is
            // too small — we mirror that behavior by passing a slice of the
            // correct length.
            let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
            let limit = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
            let capacity = limit.saturating_sub(pos).min(65_536);
            let mut buf = vec![0u8; capacity.max(1)];

            let result = {
                let reg = crate::servlet::s2_registry().lock();
                match reg.dgrams.get(&sid) {
                    Some(sock) => sock.recv_from(&mut buf),
                    None => return Ok(Some(Value::Object(None))),
                }
            };
            match result {
                Ok((n, src_addr)) => {
                    let arr = match ctx.get_field(bb, 0) {
                        Value::Object(Some(a)) => a,
                        _ => return Ok(Some(Value::Object(None))),
                    };
                    let arr_len = ctx.array_length(arr);
                    let copy_len = n.min(arr_len.saturating_sub(pos));
                    for i in 0..copy_len {
                        ctx.set_array_element(arr, pos + i, Value::Int(buf[i] as i8 as i32));
                    }
                    ctx.set_field(bb, 1, Value::Int((pos + copy_len) as i32));

                    let sa = try_alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2)?;
                    // Pin across the create_string below — a moving young GC
                    // there would relocate the fresh address (native
                    // stale-local family).
                    let sa_pin = ctx.pin_native_root(sa);
                    let host_str = ctx.create_string(&src_addr.ip().to_string());
                    let sa = ctx.read_native_pin(sa_pin, sa);
                    ctx.unpin_native_roots(sa_pin);
                    ctx.set_field(sa, 0, Value::Object(Some(host_str)));
                    ctx.set_field(sa, 1, Value::Int(src_addr.port() as i32));
                    Ok(Some(Value::Object(Some(sa))))
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    // Non-blocking datagram with no pending packet: per JDK
                    // DatagramChannel.receive, return null.
                    Ok(Some(Value::Object(None)))
                }
                Err(e) => Err(RuntimeError::IOException {
                    message: format!("DatagramChannel.receive failed: {e}"),
                }
                .into()),
            }
        },
    );

    // read(ByteBuffer) — connected-mode receive. Semantics: return the
    // number of bytes placed into the buffer, or -1 on EOF, or 0 if
    // non-blocking and no packet is pending.
    r.register(dc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.get_field(this, 2).as_int().unwrap_or(0) == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "DatagramChannel.read: not connected".into(),
            }
            .into());
        }
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if sid < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(bb))) => *bb,
            _ => return Ok(Some(Value::Int(-1))),
        };
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let limit = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let capacity = limit.saturating_sub(pos).min(65_536);
        if capacity == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut buf = vec![0u8; capacity];
        // FIX 2026-08-12 (W7-53). Two defects in one expression, and the census
        // that found `receive`'s copy of the first did not name this one.
        //
        // (1) `recv` ran with `reg` — the guard on the PROCESS-WIDE
        //     `s2_registry` mutex — still alive, because it sat inside the
        //     block that owns it. On a blocking datagram channel that parks
        //     every synthetic socket operation in the VM until a packet
        //     arrives, INCLUDING the `close` that would end the wait. The lock
        //     is the wait's own gate, so the wedge is self-sustaining. Fixed by
        //     the rule both TCP read paths have followed since AUDIT
        //     2026-05-17: take the handle out under a brief guard, drop the
        //     guard, then make the blocking call.
        //
        // (2) The park that remains is not close-aware by itself — closing one
        //     duplicated socket handle does not abort a blocking call on
        //     another duplicate. So it goes through the same
        //     poll-and-re-ask-the-registry loop as every other site in this
        //     family.
        let sock = {
            let reg = crate::servlet::s2_registry().lock();
            match reg.dgrams.get(&sid) {
                Some(sock) => match sock.try_clone() {
                    Ok(dup) => dup,
                    // A dup failure is an ordinary IO error, not a reason to
                    // fall back to receiving under the lock — that is the
                    // behaviour being removed.
                    Err(_) => return Ok(Some(Value::Int(-1))),
                },
                None => return Ok(Some(Value::Int(-1))),
            }
        };
        // `Ok(_)` — ready, or no poll primitive on this target (fall through to
        // the plain blocking `recv`). `Err(_)` — closed from another thread,
        // which this surface reports as -1, its end-of-input answer.
        if crate::servlet::s2_wait_ready_close_aware(
            crate::servlet::dgram_pollreq_fd(&sock),
            false,
            &|| crate::servlet::s2_dgram_still_registered(sid),
        )
        .is_err()
        {
            return Ok(Some(Value::Int(-1)));
        }
        let result = sock.recv(&mut buf);
        match result {
            Ok(n) => {
                let arr = match ctx.get_field(bb, 0) {
                    Value::Object(Some(a)) => a,
                    _ => return Ok(Some(Value::Int(-1))),
                };
                for i in 0..n {
                    ctx.set_array_element(arr, pos + i, Value::Int(buf[i] as i8 as i32));
                }
                ctx.set_field(bb, 1, Value::Int((pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Some(Value::Int(0))),
            Err(_) => Ok(Some(Value::Int(-1))),
        }
    });

    r.register(dc, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.get_field(this, 2).as_int().unwrap_or(0) == 0 {
            return Err(RuntimeError::IllegalStateException {
                message: "DatagramChannel.write: not connected".into(),
            }
            .into());
        }
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if sid < 0 {
            return Ok(Some(Value::Int(0)));
        }
        let bb = match args.get(1) {
            Some(Value::Object(Some(bb))) => *bb,
            _ => return Ok(Some(Value::Int(0))),
        };
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0) as usize;
        let limit = ctx.get_field(bb, 2).as_int().unwrap_or(0) as usize;
        let remaining = limit.saturating_sub(pos);
        if remaining == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let arr = match ctx.get_field(bb, 0) {
            Value::Object(Some(a)) => a,
            _ => return Ok(Some(Value::Int(0))),
        };
        let mut buf = vec![0u8; remaining];
        for i in 0..remaining {
            if let Value::Int(b) = ctx.get_array_element(arr, pos + i) {
                buf[i] = b as u8;
            }
        }
        let result = {
            let reg = crate::servlet::s2_registry().lock();
            match reg.dgrams.get(&sid) {
                Some(sock) => sock.send(&buf),
                None => return Ok(Some(Value::Int(0))),
            }
        };
        match result {
            Ok(n) => {
                ctx.set_field(bb, 1, Value::Int((pos + n) as i32));
                Ok(Some(Value::Int(n as i32)))
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(Some(Value::Int(0))),
            Err(e) => Err(RuntimeError::IOException {
                message: format!("DatagramChannel.write failed: {e}"),
            }
            .into()),
        }
    });

    // register(Selector, int) — delegates to the canonical s2 key builder
    // so the Selector's poll path discovers this channel and polls its
    // real fd. (NEW-3: previously allocated a disconnected 3-field key
    // that the selector would never wake up on.)
    r.register(
        dc,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );
    r.register(
        dc,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel,
    );

    // Reference to SocketRegistry kept to keep the `use` alive for all
    // the nested closures that look up dgrams above.
    let _ = std::marker::PhantomData::<SocketRegistry>;

    // socket() -> DatagramSocket
    r.register(dc, "socket", "()Ljava/net/DatagramSocket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = match ctx.get_field(this, 0) {
            Value::Int(p) => p,
            _ => 0,
        };
        let ds = try_alloc_concurrent_synthetic(ctx, "java/net/DatagramSocket", 3)?;
        ctx.set_field(ds, 0, Value::Int(port));
        ctx.set_field(ds, 1, Value::Int(0)); // not closed
        ctx.set_field(ds, 2, Value::Int(0)); // timeout
        Ok(Some(Value::Object(Some(ds))))
    });

    // getLocalAddress() -> SocketAddress
    r.register(
        dc,
        "getLocalAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let port = match ctx.get_field(this, 0) {
                Value::Int(p) => p,
                _ => 0,
            };
            let sa = try_alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 2)?;
            // Pin across the create_string below — a moving young GC there
            // would relocate the fresh address (native stale-local family).
            let sa_pin = ctx.pin_native_root(sa);
            let host = ctx.create_string("0.0.0.0");
            let sa = ctx.read_native_pin(sa_pin, sa);
            ctx.unpin_native_roots(sa_pin);
            ctx.set_field(sa, 0, Value::Object(Some(host)));
            ctx.set_field(sa, 1, Value::Int(port));
            Ok(Some(Value::Object(Some(sa))))
        },
    );

    // getRemoteAddress() -> SocketAddress. The constant null said "not
    // connected" even right after a successful `connect()`, so a caller
    // checking the peer before writing saw an unconnected channel forever.
    // Read the real peer off the registry socket; null now genuinely means
    // "not connected" (which is what the JDK returns in that case).
    r.register(
        dc,
        "getRemoteAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid < 0 || ctx.get_field(this, 2).as_int().unwrap_or(0) == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let peer = {
                let reg = crate::servlet::s2_registry().lock();
                reg.dgrams.get(&sid).and_then(|s| s.peer_addr().ok())
            };
            match peer {
                Some(a) => Ok(Some(Value::Object(Some(p72_alloc_inet_socket_address(
                    ctx,
                    &a.ip().to_string(),
                    a.port() as i32,
                )?)))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );

    // setOption / getOption — previously a discard-and-return-`this` setter
    // paired with an always-null getter. A null from `getOption` is worse than
    // wrong: `NetworkChannel.getOption` is declared `<T> T`, so the caller
    // unboxes it and gets an NPE. Both halves now talk to the real UDP socket,
    // and an option this surface does not model raises the exception the JDK
    // spec names for that case instead of answering null.
    r.register(
        dc,
        "setOption",
        "(Ljava/net/SocketOption;Ljava/lang/Object;)Ljava/nio/channels/DatagramChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = dc_option_name(ctx, args.get(1).copied());
            let value = dc_option_int(ctx, args.get(2).copied());
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            if sid >= 0 {
                let reg = crate::servlet::s2_registry().lock();
                if let Some(sock) = reg.dgrams.get(&sid) {
                    let sr = socket2::SockRef::from(sock);
                    let applied = match name.as_str() {
                        "SO_BROADCAST" => sr.set_broadcast(value != 0),
                        "SO_REUSEADDR" => sr.set_reuse_address(value != 0),
                        "SO_SNDBUF" => sr.set_send_buffer_size(value.max(0) as usize),
                        "SO_RCVBUF" => sr.set_recv_buffer_size(value.max(0) as usize),
                        "IP_MULTICAST_TTL" | "IP_TTL" => sr.set_ttl(value.max(0) as u32),
                        _ => {
                            return Err(RuntimeError::UnsupportedOperationException {
                                message: format!("DatagramChannel option not supported: {name}"),
                            }
                            .into())
                        }
                    };
                    let _ = applied;
                }
            }
            Ok(Some(Value::Object(Some(this))))
        },
    );
    r.register(
        dc,
        "getOption",
        "(Ljava/net/SocketOption;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let name = dc_option_name(ctx, args.get(1).copied());
            let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
            let raw = {
                let reg = crate::servlet::s2_registry().lock();
                match reg.dgrams.get(&sid) {
                    Some(sock) => {
                        let sr = socket2::SockRef::from(sock);
                        match name.as_str() {
                            "SO_BROADCAST" => Some(sr.broadcast().map(i32::from).unwrap_or(0)),
                            "SO_REUSEADDR" => Some(sr.reuse_address().map(i32::from).unwrap_or(0)),
                            "SO_SNDBUF" => Some(sr.send_buffer_size().unwrap_or(0) as i32),
                            "SO_RCVBUF" => Some(sr.recv_buffer_size().unwrap_or(0) as i32),
                            "IP_MULTICAST_TTL" | "IP_TTL" => Some(sr.ttl().unwrap_or(0) as i32),
                            _ => None,
                        }
                    }
                    // Not bound yet: still answer the options we model, with the
                    // JDK defaults, rather than null.
                    None => match name.as_str() {
                        "SO_BROADCAST" | "SO_REUSEADDR" => Some(0),
                        "SO_SNDBUF" | "SO_RCVBUF" => Some(0),
                        "IP_MULTICAST_TTL" | "IP_TTL" => Some(1),
                        _ => None,
                    },
                }
            };
            match raw {
                Some(v) => dc_box_option(ctx, &name, v),
                None => Err(RuntimeError::UnsupportedOperationException {
                    message: format!("DatagramChannel option not supported: {name}"),
                }
                .into()),
            }
        },
    );

    // KEEP: `DatagramChannel.validOps()` is specified to return exactly
    // `SelectionKey.OP_READ | SelectionKey.OP_WRITE` (1 | 4 == 5) for every
    // datagram channel — the constant IS the JDK implementation, which also
    // returns a fixed 5.
    r.register(dc, "validOps", "()I", |_ctx, _args| Ok(Some(Value::Int(5))));

    // isBlocking() -> boolean
    //
    // Slot 3 only exists on the 5-field synthetic layout documented at the top
    // of this function. `native-io`'s fd-table-backed `DatagramChannel.open`
    // (which is registered LATER and therefore wins) allocates a 3-field
    // object and keeps its state in identity-hash side tables, so slot 3 is
    // out of bounds there — `get_field` answers `Object(None)`, i.e. "not
    // blocking", for a channel the JDK specifies as blocking ("A newly-created
    // channel is always in blocking mode", `SelectableChannel`). Fall back to
    // that documented initial state whenever the receiver carries no blocking
    // slot, instead of reporting the out-of-bounds read.
    //
    // RESIDUAL: on the fd-table layout a later `configureBlocking(false)` is
    // applied to the OS socket by `native_dc_configure_blocking` but recorded
    // nowhere Java-visible, so this still answers `true` afterwards. Closing
    // that needs a blocking flag beside the fd in `native-io`'s `dc_fds`
    // table, which is outside this module.
    r.register(dc, "isBlocking", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field(this, 3) {
            v @ Value::Int(_) => Ok(Some(v)),
            _ => Ok(Some(Value::Int(1))),
        }
    });
    r.set_category(__prev_cat);
}

// =============================================================================
// com.sun.net.httpserver — HttpServer, HttpContext, HttpExchange, HttpHandler, Headers
// =============================================================================

// `HttpContext` is a 2-slot synthetic (path=0, handler=1) whose layout is
// mirrored in `class_manager.rs::synthetic_stub_fields` and allocated from two
// separate modules, so the authenticator cannot simply take a third slot. Bind
// it in a side table keyed by (vm, identity hash), holding a global GC root so
// a moving collector cannot leave the entry pointing at a vacated address.
type HttpContextKey = (usize, i32);

fn http_context_authenticators(
) -> &'static std::sync::Mutex<std::collections::HashMap<HttpContextKey, usize>> {
    static R: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<HttpContextKey, usize>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn http_context_key(ctx: &dyn NativeContext, hctx: ObjectRef) -> HttpContextKey {
    (ctx.vm_identity(), ctx.identity_hash_code(hctx))
}

/// Bind `auth` to `hctx` and return the authenticator it replaced — the real
/// `HttpContext.setAuthenticator` hands the previous one back.
///
/// The displaced global root is deliberately left in place: the reference is
/// about to be returned to Java, and dropping the root first would hand the
/// caller an object with no root holding it.
fn http_context_set_authenticator(
    ctx: &mut dyn NativeContext,
    hctx: ObjectRef,
    auth: Option<ObjectRef>,
) -> Option<ObjectRef> {
    let key = http_context_key(ctx, hctx);
    let handle = auth
        .map(|auth| ctx.add_global_root(auth))
        .filter(|h| *h != 0);
    let previous = {
        let mut table = http_context_authenticators()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match handle {
            Some(h) => table.insert(key, h),
            None => table.remove(&key),
        }
    };
    previous.and_then(|h| ctx.resolve_global_root(h))
}

fn http_context_authenticator(ctx: &dyn NativeContext, hctx: ObjectRef) -> Option<ObjectRef> {
    let key = http_context_key(ctx, hctx);
    let handle = http_context_authenticators()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied();
    handle.and_then(|h| ctx.resolve_global_root(h))
}

/// Generic (vm, identity-hash) -> global-root side table, used for the two
/// `com.sun.net.httpserver` back-references the phase-72 synthetic layouts have
/// no slot for: `HttpServer`'s executor and `HttpContext`'s owning server.
///
/// Same shape and the same GC contract as `http_context_authenticators` above:
/// the stored value is a GLOBAL ROOT handle, not a raw `ObjectRef`, so a moving
/// collector cannot leave the entry pointing at a vacated address.
fn http_object_links(
) -> &'static std::sync::Mutex<std::collections::HashMap<(u8, HttpContextKey), usize>> {
    static R: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<(u8, HttpContextKey), usize>>,
    > = std::sync::OnceLock::new();
    R.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

const HTTP_LINK_EXECUTOR: u8 = 0;
pub(crate) const HTTP_LINK_CONTEXT_SERVER: u8 = 1;
/// Per-`HttpExchange` attribute map (`get/setAttribute`).
const HTTP_LINK_EXCHANGE_ATTRS: u8 = 2;
/// Per-`HttpContext` attribute map (`getAttributes`).
const HTTP_LINK_CONTEXT_ATTRS: u8 = 3;

pub(crate) fn http_link_set(
    ctx: &mut dyn NativeContext,
    kind: u8,
    owner: ObjectRef,
    target: Option<ObjectRef>,
) {
    let key = (kind, http_context_key(ctx, owner));
    let handle = target
        .map(|target| ctx.add_global_root(target))
        .filter(|h| *h != 0);
    let previous = {
        let mut table = http_object_links()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match handle {
            Some(h) => table.insert(key, h),
            None => table.remove(&key),
        }
    };
    // Nothing is handed back to Java here (unlike `setAuthenticator`), so the
    // displaced root can and must be released.
    if let Some(h) = previous {
        ctx.remove_global_root(h);
    }
}

fn http_link_get(ctx: &dyn NativeContext, kind: u8, owner: ObjectRef) -> Option<ObjectRef> {
    let key = (kind, http_context_key(ctx, owner));
    let handle = http_object_links()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .copied();
    handle.and_then(|h| ctx.resolve_global_root(h))
}

/// The attribute `HashMap` bound to `owner` under `kind`, created on first use.
///
/// `com.sun.net.httpserver` attributes have no slot in either synthetic layout
/// (and `HttpExchange` is allocated by `net_phase_e`, whose slots this file
/// does not own), so they live in the same global-root-backed side table as
/// the executor / owning-server links above.
fn http_attribute_map(
    ctx: &mut dyn NativeContext,
    kind: u8,
    owner: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(existing) = http_link_get(ctx, kind, owner) {
        return Ok(existing);
    }
    // Pin across the map alloc/init — a moving young GC there would relocate
    // `owner` (native stale-local family), and the link table keys on its
    // identity hash.
    let owner_pin = ctx.pin_native_root(owner);
    let map = try_alloc_concurrent_synthetic(ctx, "java/util/HashMap", 3)?;
    let map_pin = ctx.pin_native_root(map);
    cratonvm_native_collections::native_map_init(ctx, &[Value::Object(Some(map))]).ok();
    let owner = ctx.read_native_pin(owner_pin, owner);
    let map = ctx.read_native_pin(map_pin, map);
    http_link_set(ctx, kind, owner, Some(map));
    // `http_link_set` took a global root on the map, so it stays reachable
    // after the pins below are dropped.
    let map = ctx.read_native_pin(map_pin, map);
    ctx.unpin_native_roots(owner_pin);
    Ok(map)
}

/// `HttpExchange.getAttribute(String)`.
fn http_exchange_get_attribute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let Some(map) = http_link_get(ctx, HTTP_LINK_EXCHANGE_ATTRS, this) else {
        // Nothing was ever set on this exchange — no map, and no allocation
        // just to answer a miss.
        return Ok(Some(Value::Object(None)));
    };
    let found =
        cratonvm_native_collections::native_map_get_pub(ctx, &[Value::Object(Some(map)), key])?;
    Ok(Some(found.unwrap_or(Value::Object(None))))
}

/// `HttpExchange.setAttribute(String, Object)` — the missing half of the pair.
///
/// A null value removes the binding, matching `ExchangeImpl`, which stores
/// attributes in a plain `Map` and therefore treats `put(k, null)` as "no
/// value" for the `getAttribute` that follows.
fn http_exchange_set_attribute(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key = args.get(1).copied().unwrap_or(Value::Object(None));
    let val = args.get(2).copied().unwrap_or(Value::Object(None));
    // Pin the key/value across the (possibly allocating) map creation. Both
    // pins are taken BEFORE `http_attribute_map`'s own pin, so its
    // `unpin_native_roots` truncation cannot drop them.
    let key_pin = pinned_object_value(ctx, key);
    let val_pin = pinned_object_value(ctx, val);
    let map = http_attribute_map(ctx, HTTP_LINK_EXCHANGE_ATTRS, this);
    let key = read_pinned_object_value(ctx, key_pin, key);
    let val = read_pinned_object_value(ctx, val_pin, val);
    let result = if matches!(val, Value::Object(None)) {
        cratonvm_native_collections::native_map_remove_pub(ctx, &[Value::Object(Some(map?)), key])
            .map(|_| ())
    } else {
        cratonvm_native_collections::native_map_put_pub(ctx, &[Value::Object(Some(map?)), key, val])
            .map(|_| ())
    };
    if let Some((handle, _)) = key_pin {
        ctx.unpin_native_roots(handle);
    } else if let Some((handle, _)) = val_pin {
        ctx.unpin_native_roots(handle);
    }
    result?;
    Ok(None)
}

pub(crate) fn register_p72_http_server(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Wave 3-B (RE.4): the real HttpServer/HttpExchange implementations live in
    // `net_phase_e::register_re10_http_server`, which actually binds a TcpListener
    // and dispatches HTTP/1.1 round-trips. Phase 72 used to register opaque stubs
    // for the same `(class, method, descriptor)` keys; because Phase 72 runs AFTER
    // Phase E, those stubs silently overrode the real impls and broke
    // `URL.openConnection().getInputStream()` round-trips that target a loopback
    // HttpServer. We keep ONLY the HttpServerImpl alias for `create` (Phase E
    // registers `HttpServer` but not the impl class), the no-arg `create()`
    // factory, executor accessors, bind, and the HttpContext / HttpHandler /
    // Headers helpers that Phase E does not cover. (`removeContext` was on that
    // list until wave 3 found it shadowing Phase E's real one on the same key —
    // see below.)
    //
    // Wave 4: `create()` (both arities) and `bind` now DELEGATE to Phase E
    // rather than reimplementing. Every one of them used to mint (or mutate) a
    // 3-slot carrier that had no `server_registry` entry, which is a server
    // `start()` can never start ("server not registered") and `bind()` can never
    // bind. Only the bodies moved — the keys stay registered here, because Phase
    // 72 runs after Phase E and would otherwise shadow them again with the old
    // behaviour.
    let hs = "com/sun/net/httpserver/HttpServer";
    // `sun/...`, NOT `com/sun/...`. The JDK's implementation class is
    // `sun.net.httpserver.HttpServerImpl`, which is what
    // `net_phase_e::HS_IMPL_CLASS` spells and what both factories mint. This
    // read `com/sun/net/httpserver/HttpServerImpl` -- a class that exists in no
    // JDK and that nothing in this VM allocates -- so every row registered on
    // it below was unreachable from the day it was written. `class_manager.rs`
    // found the same typo in the field-count table and fixed it there by
    // listing BOTH spellings; the REGISTRATION half was left behind, which is
    // why a `--dump-native-registry` census reports zero rows for either
    // spelling of the impl class.
    let hs_simple = "sun/net/httpserver/HttpServerImpl";

    // HttpServerImpl alias — Phase E registers HttpServer; route the impl class
    // to the same field layout so that invocations via `HttpServerImpl.create`
    // do not fall through to a missing-native error.
    r.register(
        hs_simple,
        "create",
        "(Ljava/net/InetSocketAddress;I)Lcom/sun/net/httpserver/HttpServer;",
        // Delegated to Phase E's factory (wave 4). The local body used to mint
        // a 3-slot object with no `server_registry` entry and — despite taking
        // an `InetSocketAddress` — never opened a socket, so `start()` on the
        // result failed with "server not registered". Phase E's factory both
        // binds and registers; the receiver keeps the PUBLIC class name so the
        // `HttpServer`-keyed natives registered in this file (which Phase E's
        // `alias_class` snapshot cannot see) still dispatch on it.
        |ctx, args| {
            // Mint the IMPL class, like phase E's own factory. This passed the
            // PUBLIC name so that the `HttpServer`-keyed rows registered later
            // in this file would still dispatch on the receiver -- but those
            // rows are now registered on the impl class too (see `hs_simple`
            // above, whose spelling was wrong until 2026-09-02), so the
            // receiver reaches them either way. Minting the public name handed
            // the application an instance of an ABSTRACT class.
            crate::net_phase_e::re10_create_server(ctx, args, "sun/net/httpserver/HttpServerImpl")
        },
    );

    // No-arg factory not covered by Phase E's 2-arg `create`. Phase E now owns
    // the body: it allocates the same 6-slot carrier and registers the
    // `ServerState` that `bind`/`start`/`stop`/`createContext` all look up.
    // Per the JDK, `create()` returns an UNBOUND server — `bind()` must be
    // called before `start()`.
    for cls in [hs, hs_simple] {
        r.register(
            cls,
            "create",
            "()Lcom/sun/net/httpserver/HttpServer;",
            |ctx, _args| crate::net_phase_e::re10_create_unbound_server(ctx),
        );
        r.register(
            cls,
            "createContext",
            "(Ljava/lang/String;)Lcom/sun/net/httpserver/HttpContext;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let path = args.get(1).copied().unwrap_or(Value::Object(None));
                // Pin across the context alloc below — a moving young GC there
                // would relocate it (native stale-local family).
                let this_pin = ctx.pin_native_root(this);
                let path_pin = pinned_object_value(ctx, path);
                let hctx =
                    try_alloc_concurrent_synthetic(ctx, "com/sun/net/httpserver/HttpContext", 2)?;
                let path = read_pinned_object_value(ctx, path_pin, path);
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field(hctx, 0, path);
                ctx.set_field(hctx, 1, Value::Object(None));
                if let Some((h, _)) = path_pin {
                    ctx.unpin_native_roots(h);
                }
                // Record the owning server so `HttpContext.getServer()` can
                // answer it (the 2-slot context layout has no room for it).
                http_link_set(ctx, HTTP_LINK_CONTEXT_SERVER, hctx, Some(this));
                ctx.unpin_native_roots(this_pin);
                Ok(Some(Value::Object(Some(hctx))))
            },
        );
        // set/getExecutor were a discard-then-null pair on this layout: the
        // setter dropped the executor and the getter always answered null, so
        // `server.setExecutor(pool); server.getExecutor()` returned null and a
        // caller that dispatches through the returned executor NPEs. The
        // side table keeps working whatever the carrier's slot count is, so it
        // stayed put when the factories were unified onto net_phase_e's 6-slot
        // layout (which does have an `HS_EXECUTOR` slot; its own set/getExecutor
        // pair still serves receivers classed `sun/net/httpserver/HttpServerImpl`).
        // Slot 1 is `started` in both layouts, so the guard below is unchanged.
        r.register(
            cls,
            "setExecutor",
            "(Ljava/util/concurrent/Executor;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                if ctx.object_num_fields(this) > 1
                    && ctx.get_field(this, 1).as_int().unwrap_or(0) != 0
                {
                    return Err(RuntimeError::IllegalStateException {
                        message: "server already started".into(),
                    }
                    .into());
                }
                let exec = match args.get(1).copied() {
                    Some(Value::Object(o)) => o,
                    _ => None,
                };
                http_link_set(ctx, HTTP_LINK_EXECUTOR, this, exec);
                Ok(None)
            },
        );
        r.register(
            cls,
            "getExecutor",
            "()Ljava/util/concurrent/Executor;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(Value::Object(http_link_get(
                    ctx,
                    HTTP_LINK_EXECUTOR,
                    this,
                ))))
            },
        );
        // `bind` used to store the address in slot 0 and stop there, so the
        // ONLY sequence the no-arg factory supports —
        // `create(); bind(addr, backlog); start();` — never opened a socket and
        // `start()` reported the server unbound. Phase E's version really binds
        // (and updates `getAddress()`/`getPort()` with the port the OS chose,
        // which matters for the `bind(new InetSocketAddress(0), 0)` idiom).
        r.register(
            cls,
            "bind",
            "(Ljava/net/InetSocketAddress;I)V",
            |ctx, args| crate::net_phase_e::re10_bind_server(ctx, args),
        );
        // DELETED (wave 3) — both `removeContext` overloads used to be no-ops
        // here, on the premise that the real ones in
        // `net_phase_e::register_re10_http_server` are aliased onto a distinct
        // `sun/net/httpserver/HttpServerImpl` receiver and so could not be
        // shadowed. That premise is false: net_phase_e registers them on
        // `com/sun/net/httpserver/HttpServer` (net_phase_e.rs `let hs = …`),
        // the SAME key this loop used, and phase 72 runs AFTER phase E
        // (lib.rs: `register_phase_e_networking` then `register_phase72_natives`),
        // so last-writer-wins made the no-op shadow the real implementation for
        // EVERY server — including the `HttpServer.create(addr, backlog)` ones
        // whose routes it is supposed to edit (ES MultipleHosts
        // `resetWaitHandlers`). Dropping these two restores it.
        //
        // Wave 4 addendum: this phase's `create()` no longer mints a carrier of
        // its own, so phase E's `removeContext` now finds a real registry entry
        // for THESE servers too and edits their routes for real, instead of
        // missing the lookup and no-op-ing as it did while the id slot was
        // never written.
    }

    // The HttpContext surface is shared with phase E so it exists in every
    // mode; see `register_http_context_surface`. Registered again here
    // because phase 72 runs last under --synthetic-jdk and would
    // otherwise leave phase E's rows owning the slots -- same bodies, so
    // the winner does not matter, but the ownership stays where the
    // surrounding comments say it is.
    register_http_context_surface(r);

    let hex = "com/sun/net/httpserver/HttpExchange";
    // MOVED to `net_phase_e::register_re10_http_server`, which is the
    // real-JDK-live registrar and the one that MINTS the exchange
    // (`re10_dispatch_pending` captures `local_addr()`/`peer_addr()` into
    // `HEX_LOCAL_ADDR`/`HEX_REMOTE_ADDR`). Registering the consumer half here —
    // in a `#[cfg(feature = "synthetic-jdk")]` registrar — meant the producer
    // ran in the default build while the getters did not exist there at all, so
    // `getLocalAddress()`/`getRemoteAddress()` resolved to the abstract
    // `com.sun.net.httpserver.HttpExchange` declaration and threw
    // `AbstractMethodError`. Producer and consumer now live together.

    // `getAttribute` was constant-null only because its partner did not exist:
    // no `HttpExchange.setAttribute` was registered anywhere in the tree, so
    // no key could ever have been set. Registering the PAIR is what makes both
    // real — the attribute map hangs off the same global-root side table as the
    // other `com.sun.net.httpserver` links, because the exchange's slots are
    // owned by `net_phase_e` and this file must not claim one.
    r.register(
        hex,
        "getAttribute",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        http_exchange_get_attribute,
    );
    r.register(
        hex,
        "setAttribute",
        "(Ljava/lang/String;Ljava/lang/Object;)V",
        http_exchange_set_attribute,
    );

    // `HttpHandler` is an interface, so under a real JDK the declaring class
    // of a resolved `handle` is the user's implementation and this entry is
    // never reached for it; it fires only for a synthetic receiver that has
    // no implementation anywhere (and in `--synthetic-jdk` mode there is no
    // interface bytecode to fall through to either). The old no-op left the
    // request dispatcher in `net_phase_e` reading the exchange's untouched
    // status slot, so an unhandled request answered `200 OK` with an empty
    // body — the one reply that is certainly wrong. Report the server-side
    // failure instead, through the exchange's own natives so this works
    // against a real `HttpExchange` as well as the synthetic one.
    r.register(
        "com/sun/net/httpserver/HttpHandler",
        "handle",
        "(Lcom/sun/net/httpserver/HttpExchange;)V",
        |ctx, args| {
            let Some(Value::Object(Some(exchange))) = args.get(1).copied() else {
                return Ok(None);
            };
            // `-1` is the com.sun.net.httpserver convention for "no response
            // body". Errors are swallowed: failing to report the failure must
            // not turn into a second, different failure on the caller.
            //
            // KEPT SWALLOW, NARROWED on the `close`. This is a backstop for a
            // handler that is not there, so `HttpHandler.handle` has no JDK
            // body to copy a `catch` from — the swallow is a deliberate
            // decision of ours and stays. What does not belong inside it is an
            // `Error`: a `NoSuchMethodError` from `HttpExchange.close` means
            // our own `HttpExchange` natives are broken, which is neither the
            // original failure nor a second one caused by reporting it.
            // W7-57-close-flush-swallow-sweep.md
            let _ = ctx.invoke_virtual(
                exchange,
                "sendResponseHeaders",
                "(IJ)V",
                &[Value::Int(500), Value::Long(-1)],
            );
            let closed = ctx.invoke_virtual(exchange, "close", "()V", &[]);
            cratonvm_native_api::delegated_close::vm_only_best_effort(&*ctx, closed)?;
            Ok(None)
        },
    );

    // Headers = HashMap pattern (3-field)
    let hdrs = "com/sun/net/httpserver/Headers";
    r.register(hdrs, "<init>", "()V", |ctx, args| {
        cratonvm_native_collections::native_map_init(ctx, args)
    });
    r.register(
        hdrs,
        "get",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_get_pub,
    );
    r.register(
        hdrs,
        "put",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        cratonvm_native_collections::native_map_put_pub,
    );
    r.register(
        hdrs,
        "containsKey",
        "(Ljava/lang/Object;)Z",
        cratonvm_native_collections::native_map_contains_key_pub,
    );
    r.register(
        hdrs,
        "size",
        "()I",
        cratonvm_native_collections::native_map_size_pub,
    );
    r.register(
        hdrs,
        "getFirst",
        "(Ljava/lang/String;)Ljava/lang/String;",
        cratonvm_native_collections::native_map_get_pub,
    );
    r.register(
        hdrs,
        "add",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            cratonvm_native_collections::native_map_put_pub(ctx, args)?;
            Ok(None)
        },
    );
    r.register(
        hdrs,
        "set",
        "(Ljava/lang/String;Ljava/lang/String;)V",
        |ctx, args| {
            cratonvm_native_collections::native_map_put_pub(ctx, args)?;
            Ok(None)
        },
    );
    r.set_category(__prev_cat);
}

// =============================================================================
// java.net.ServerSocket extras + java.net.Socket extras (not already registered)
// =============================================================================

pub(crate) fn p72_socket_adaptor_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    local: bool,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let sc = match ctx.get_field_by_name(this, "sc") {
        Value::Object(Some(sc)) => sc,
        _ => return Ok(Some(Value::Object(None))),
    };
    let method = if local {
        "getLocalAddress"
    } else {
        "getRemoteAddress"
    };
    let socket_addr = match ctx.invoke_virtual(sc, method, "()Ljava/net/SocketAddress;", &[])? {
        Some(Value::Object(Some(addr))) => addr,
        _ => return Ok(Some(Value::Object(None))),
    };
    p72_inet_from_socket_address(ctx, socket_addr)
}

pub(crate) fn p72_inet_from_socket_address(
    ctx: &mut dyn NativeContext,
    socket_addr: ObjectRef,
) -> MethodCallResult {
    // `socket_addr` is a parameter used by all three accessors below; each is
    // real Java that can collect and move it.
    let sa_pin = ctx.pin_native_root(socket_addr);
    if let Ok(Some(Value::Object(Some(addr)))) =
        ctx.invoke_virtual(socket_addr, "getAddress", "()Ljava/net/InetAddress;", &[])
    {
        return Ok(Some(Value::Object(Some(addr))));
    }
    let socket_addr = ctx.read_native_pin(sa_pin, socket_addr);

    let host = match ctx.invoke_virtual(socket_addr, "getHostString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    let host = if host.is_empty() {
        match ctx.invoke_virtual(socket_addr, "getHostName", "()Ljava/lang/String;", &[]) {
            Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
            _ => String::new(),
        }
    } else {
        host
    };
    if host.is_empty() {
        return Ok(Some(Value::Object(None)));
    }

    let ip = host.trim_matches(&['[', ']'][..]);
    Ok(Some(Value::Object(Some(
        crate::net_phase_e::alloc_inet_address_unnamed(ctx, ip)?,
    ))))
}

/// Half-close flags for the phase-72 `java.net.Socket` surface, keyed by the
/// s2 stream id (a plain `i32` handle, so this table holds no heap references
/// and needs no GC root). `(input_shutdown, output_shutdown)`.
fn p72_socket_shutdowns() -> &'static parking_lot::Mutex<rustc_hash::FxHashMap<i32, (bool, bool)>> {
    static T: std::sync::OnceLock<parking_lot::Mutex<rustc_hash::FxHashMap<i32, (bool, bool)>>> =
        std::sync::OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(rustc_hash::FxHashMap::default()))
}

fn p72_mark_shutdown(stream_id: i32, input: bool) {
    if stream_id < 0 {
        return;
    }
    let mut t = p72_socket_shutdowns().lock();
    let e = t.entry(stream_id).or_insert((false, false));
    if input {
        e.0 = true;
    } else {
        e.1 = true;
    }
}

fn p72_shutdown_state(stream_id: i32) -> (bool, bool) {
    if stream_id < 0 {
        return (false, false);
    }
    p72_socket_shutdowns()
        .lock()
        .get(&stream_id)
        .copied()
        .unwrap_or((false, false))
}

/// `(ip, port)` of a phase-72 `java.net.Socket`'s live s2 stream, or `None`
/// when the socket has no stream (never connected / already closed).
/// `local == true` reads the local end, `false` the peer.
fn p72_socket_stream_addr(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    local: bool,
) -> Option<(String, i32)> {
    let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
    if stream_id < 0 {
        return None;
    }
    let reg = crate::servlet::s2_registry().lock();
    let stream = reg.streams.get(&stream_id)?;
    let addr = if local {
        stream.local_addr().ok()?
    } else {
        stream.peer_addr().ok()?
    };
    Some((addr.ip().to_string(), addr.port() as i32))
}

/// Accept one connection on `this` ServerSocket's underlying `TcpListener` and
/// populate `target_socket`'s stream slot. Shared by `ServerSocket.accept()`
/// and the JDK-internal `implAccept` hook, so both agree on where the listener
/// id lives (slot 3, written by this module's `bind`).
///
/// MAY BLOCK: waits for a peer, inside a blocking region.
fn p72_impl_accept(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    target_socket: ObjectRef,
) -> MethodCallResult {
    use crate::servlet::{s2_alloc_stream, s2_registry};
    let listener_id = ctx.get_field(this, 3).as_int().unwrap_or(-1); // SS_LISTENER_ID
    if listener_id < 0 {
        return Err(RuntimeError::IOException {
            message: "ServerSocket not bound".into(),
        }
        .into());
    }
    // Clone the listener handle out under a SHORT lock, then release the
    // s2_registry lock BEFORE the blocking accept() — holding it across a
    // blocking accept() deadlocks every other synthetic-socket op
    // process-wide (see the matching fix in net_phase_e::re2_accept_into).
    let listener = {
        let reg = s2_registry().lock();
        reg.listeners
            .get(&listener_id)
            .ok_or_else(|| RuntimeError::IOException {
                message: "Listener not found".into(),
            })?
            .try_clone()
            .map_err(|e| RuntimeError::IOException {
                message: format!("accept try_clone: {e}"),
            })?
    };
    let _ = listener.set_nonblocking(false);
    // GC-safety (STW blocking-region family): `accept()` waits for a peer with
    // no upper bound. Outside a blocking region the thread is neither at a
    // safepoint nor GC-cooperative, so a stop-the-world collection stalls the
    // whole VM until a client happens to connect. `target_socket` is live
    // across the wait and is re-read through the region's fixup, because a
    // collection completing inside the wait relocates it.
    ctx.begin_blocking_region();
    let accepted = listener.accept();
    let mut refs = [Value::Object(Some(target_socket))];
    ctx.end_blocking_region_refs(&mut refs);
    let target_socket = match refs[0] {
        Value::Object(Some(moved)) => moved,
        _ => target_socket,
    };
    let stream = match accepted {
        Ok((stream, _addr)) => stream,
        Err(e) => {
            return Err(RuntimeError::IOException {
                message: format!("accept failed: {e}"),
            }
            .into())
        }
    };
    let stream_id = s2_alloc_stream(stream);
    // Populate the target Socket's fields. Socket layout:
    // host=0, port=1, localPort=2, closed=3, stream_id=4.
    ctx.set_field(target_socket, 3, Value::Int(0)); // not closed
    ctx.set_field(target_socket, 4, Value::Int(stream_id));
    Ok(None)
}

pub(crate) fn register_p72_server_socket(r: &mut NativeMethodRegistry) {
    // NIO-SERVER-SOCKET (route 1): skip the synthetic java.net.Socket/ServerSocket
    // surface so real bytecode drives sun/nio/ch/Net. Third of three registrars
    // (with phases_early::register_phase53_socket_stubs and
    // net_phase_e::register_re1_socket/register_re2_server_socket). See
    // `reference_server_socket_gap`.
    if crate::vmflags().io.real_net_sockets {
        return;
    }
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // ServerSocket extras — add methods not registered in phase 53
    let ss = "java/net/ServerSocket";
    r.register(ss, "<init>", "(IILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = match args.get(1) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        let backlog = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 50,
        };
        ctx.set_field(this, 0, Value::Int(port));
        ctx.set_field(this, 1, Value::Int(backlog));
        ctx.set_field(this, 2, Value::Int(0));
        Ok(None)
    });
    // accept() previously returned a constant null. That is the worst possible
    // answer for a server loop: `while (true) handle(server.accept())` neither
    // blocks nor fails — it spins at full speed handing out nulls, and the
    // caller NPEs (or, with a null check, busy-loops forever). The real accept
    // already existed as this module's `implAccept` hook; both now share
    // `p72_impl_accept`, so `accept()` does what the JDK's own does — allocate
    // the peer `Socket`, then delegate.
    //
    // This registration shadows `net_phase_e::register_re2_server_socket`'s real
    // `accept` (phase 72 runs later), but the two are not interchangeable: this
    // module's `bind` — which also wins — stores the listener id in slot 3,
    // whereas re2's accept reads it from its own identity-keyed side table. So
    // deleting this entry would have produced an accept that never finds the
    // listener. Sharing one helper keeps bind and accept on one storage scheme.
    //
    // RESIDUAL: a channel-backed wrapper (from `ServerSocketChannel.socket()`,
    // whose listener fd lives on the channel at slot 4 -> ssc slot 2, not in the
    // s2 listener table) now raises `IOException: ServerSocket not bound`
    // instead of returning null. That is an honest failure for a path that was
    // already broken, not a new capability regression.
    //
    // MAY BLOCK: `p72_impl_accept` waits for a connection (blocking region
    // inside).
    r.register(ss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Socket layout: host=0, port=1, localPort=2, closed=3, stream_id=4.
        let this_pin = ctx.pin_native_root(this);
        let sock0 = try_alloc_concurrent_synthetic(ctx, "java/net/Socket", 5)?;
        let sock_pin = ctx.pin_native_root(sock0);
        let this = ctx.read_native_pin(this_pin, this);
        let sock = ctx.read_native_pin(sock_pin, sock0);
        ctx.set_field(sock, 0, Value::Object(None));
        ctx.set_field(sock, 1, Value::Int(0));
        ctx.set_field(sock, 2, Value::Int(0));
        ctx.set_field(sock, 3, Value::Int(0));
        ctx.set_field(sock, 4, Value::Int(-1));
        let result = p72_impl_accept(ctx, this, sock);
        let sock = ctx.read_native_pin(sock_pin, sock);
        ctx.unpin_native_roots(this_pin);
        result?;
        Ok(Some(Value::Object(Some(sock))))
    });
    r.register(ss, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Channel-backed wrapper (created by ServerSocketChannel.socket()) has a 5th
        // field holding a back-ref to the SocketChannel. Delegate the bind to the
        // channel so the underlying TCP listener lives in fd_table (visible to the
        // selector). For a plain ServerSocket, keep using s2_alloc_listener.
        if ctx.object_num_fields(this) >= 5 {
            if let Value::Object(Some(ssc)) = ctx.get_field(this, 4) {
                let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
                let addr_str = p98_extract_socket_addr(ctx, addr_obj);
                // GAP I6 — server-socket bind is its own authority.
                match crate::capability_gate::open_tcp_listener_gated(&*ctx, &addr_str) {
                    Ok(fd) => {
                        ctx.set_field(ssc, 1, Value::Int(1));
                        ctx.set_field(ssc, 2, Value::Int(fd as i32));
                        let port = ctx
                            .fd_table()
                            .tcp_local_addr(fd)
                            .ok()
                            .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
                            .unwrap_or(0);
                        ctx.set_field(this, 0, Value::Int(port)); // SS_PORT mirror
                        return Ok(None);
                    }
                    Err(e) => {
                        return Err(crate::capability_gate::translate_open_failure(e, |io| {
                            format!("bind {addr_str}: {io}")
                        }));
                    }
                }
            }
        }
        // Plain ServerSocket path — bind a real TcpListener via s2_alloc_listener.
        // GAP I6 (second half): this path never touches `fd_table`, so it needs
        // the bare endpoint gate — `open_tcp_listener_gated` cannot reach it.
        use crate::servlet::s2_alloc_listener;
        use std::net::TcpListener;
        let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
        let addr_str = p98_extract_socket_addr(ctx, addr_obj);
        crate::capability_gate::gate_network(&*ctx, &addr_str)?;
        match TcpListener::bind(&addr_str) {
            Ok(listener) => {
                let actual_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                let id = s2_alloc_listener(listener);
                ctx.set_field(this, 0, Value::Int(actual_port));
                ctx.set_field(this, 3, Value::Int(id));
            }
            Err(e) => {
                return Err(RuntimeError::IOException {
                    message: format!("bind {addr_str}: {e}"),
                }
                .into());
            }
        }
        Ok(None)
    });
    r.register(ss, "bind", "(Ljava/net/SocketAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Int(backlog)) = args.get(2) {
            ctx.set_field(this, 1, Value::Int(*backlog));
        }
        // Reuse single-arg bind path
        if ctx.object_num_fields(this) >= 5 {
            if let Value::Object(Some(ssc)) = ctx.get_field(this, 4) {
                let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
                let addr_str = p98_extract_socket_addr(ctx, addr_obj);
                // GAP I6 — see the 1-arg `bind` above.
                if let Ok(fd) = crate::capability_gate::open_tcp_listener_gated(&*ctx, &addr_str) {
                    ctx.set_field(ssc, 1, Value::Int(1));
                    ctx.set_field(ssc, 2, Value::Int(fd as i32));
                    let port = ctx
                        .fd_table()
                        .tcp_local_addr(fd)
                        .ok()
                        .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
                        .unwrap_or(0);
                    ctx.set_field(this, 0, Value::Int(port));
                }
                return Ok(None);
            }
        }
        // GAP I6 (second half) — this path never touches `fd_table`.
        use crate::servlet::s2_alloc_listener;
        use std::net::TcpListener;
        let addr_obj = args.get(1).copied().unwrap_or(Value::Object(None));
        let addr_str = p98_extract_socket_addr(ctx, addr_obj);
        crate::capability_gate::gate_network(&*ctx, &addr_str)?;
        match TcpListener::bind(&addr_str) {
            Ok(listener) => {
                let actual_port = listener.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                let id = s2_alloc_listener(listener);
                ctx.set_field(this, 0, Value::Int(actual_port));
                ctx.set_field(this, 3, Value::Int(id));
                Ok(None)
            }
            Err(e) => Err(RuntimeError::IOException {
                message: format!("bind {addr_str}: {e}"),
            }
            .into()),
        }
    });
    r.register(ss, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // For channel-backed wrappers, recompute from the channel's fd.
        if ctx.object_num_fields(this) >= 5 {
            if let Value::Object(Some(ssc)) = ctx.get_field(this, 4) {
                let fd = ctx.get_field(ssc, 2).as_int().unwrap_or(-1);
                if fd >= 0 {
                    let port = ctx
                        .fd_table()
                        .tcp_local_addr(fd as u32)
                        .ok()
                        .and_then(|s| s.rsplit(':').next().and_then(|p| p.parse::<i32>().ok()))
                        .unwrap_or(0);
                    return Ok(Some(Value::Int(port)));
                }
            }
        }
        Ok(Some(ctx.get_field(this, 0)))
    });
    // isBound() reported `true` for every ServerSocket including a freshly
    // constructed, never-bound one, so `if (!ss.isBound()) ss.bind(addr);`
    // skipped the bind and the following accept had no listener. Report the
    // real state, from the same two places this module's `bind` writes it: the
    // channel back-ref's fd (slot 4 -> ssc slot 2) for a wrapper handed out by
    // `ServerSocketChannel.socket()`, else the s2 listener id in slot 3.
    r.register(ss, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if ctx.object_num_fields(this) >= 5 {
            if let Value::Object(Some(ssc)) = ctx.get_field(this, 4) {
                let fd = ctx.get_field(ssc, 2).as_int().unwrap_or(-1);
                return Ok(Some(Value::Int(if fd >= 0 { 1 } else { 0 })));
            }
        }
        let lid = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(if lid >= 0 { 1 } else { 0 })))
    });
    r.register(
        ss,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let local_ip = if ctx.object_num_fields(this) >= 5 {
                if let Value::Object(Some(ssc)) = ctx.get_field(this, 4) {
                    let fd = ctx.get_field(ssc, 2).as_int().unwrap_or(-1);
                    if fd >= 0 {
                        ctx.fd_table().tcp_local_addr(fd as u32).ok().and_then(|s| {
                            s.rsplit_once(':')
                                .map(|(host, _)| host.trim_matches(&['[', ']'][..]).to_string())
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                let lid = ctx.get_field(this, 3).as_int().unwrap_or(-1);
                if lid >= 0 {
                    let reg = crate::servlet::s2_registry().lock();
                    reg.listeners
                        .get(&lid)
                        .and_then(|l| l.local_addr().ok())
                        .map(|a| a.ip().to_string())
                } else {
                    None
                }
            };
            match local_ip {
                Some(ip) => Ok(Some(Value::Object(Some(
                    crate::net_phase_e::alloc_inet_address_unnamed(ctx, &ip)?,
                )))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(
        ss,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let local = if ctx.object_num_fields(this) >= 5 {
                if let Value::Object(Some(ssc)) = ctx.get_field(this, 4) {
                    let fd = ctx.get_field(ssc, 2).as_int().unwrap_or(-1);
                    if fd >= 0 {
                        ctx.fd_table().tcp_local_addr(fd as u32).ok().and_then(|s| {
                            s.rsplit_once(':').and_then(|(host, port)| {
                                port.parse::<i32>()
                                    .ok()
                                    .map(|p| (host.trim_matches(&['[', ']'][..]).to_string(), p))
                            })
                        })
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                let lid = ctx.get_field(this, 3).as_int().unwrap_or(-1);
                if lid >= 0 {
                    let reg = crate::servlet::s2_registry().lock();
                    reg.listeners
                        .get(&lid)
                        .and_then(|l| l.local_addr().ok())
                        .map(|a| (a.ip().to_string(), a.port() as i32))
                } else {
                    None
                }
            };
            match local {
                Some((ip, port)) => {
                    let isa = try_alloc_concurrent_synthetic(ctx, "java/net/InetSocketAddress", 3)?;
                    let holder = try_alloc_concurrent_synthetic(
                        ctx,
                        "java/net/InetSocketAddress$InetSocketAddressHolder",
                        3,
                    )?;
                    let host = ctx.create_string(&ip);
                    let addr = crate::net_phase_e::alloc_inet_address_unnamed(ctx, &ip)?;
                    ctx.set_field(holder, 0, Value::Object(Some(host)));
                    ctx.set_field(holder, 1, Value::Object(Some(addr)));
                    ctx.set_field(holder, 2, Value::Int(port));
                    ctx.set_field(isa, 0, Value::Object(Some(holder)));
                    ctx.set_field(isa, 1, Value::Int(port));
                    ctx.set_field(isa, 2, Value::Object(Some(addr)));
                    Ok(Some(Value::Object(Some(isa))))
                }
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    // REMOVED (stub-removal wave 2): `ServerSocket.getSoTimeout()I` was a
    // constant 0 registered here, and phase 72 runs after
    // `net_phase_e::register_re2_server_socket` — so it silently replaced re2's
    // REAL getter (`re2_accept_timeout_for(listener_id)`), the one that pairs
    // with re2's `setSoTimeout` (this module registers no setter of its own).
    // The result was a socket whose SO_TIMEOUT could be set and never read
    // back: `ss.setSoTimeout(5000); ss.getSoTimeout()` answered 0, and any
    // caller that restores a previous timeout around a call restored 0
    // (= block forever) instead. Dropping this entry lets the matching pair
    // win again.

    // ServerSocket setReuseAddress/setReceiveBufferSize: use real socket options
    // Note: ServerSocket doesn't always have a stream_id, so we track these as fields if needed
    // For now, these are kept as field-tracking stubs since ServerSocket doesn't expose
    // the underlying listener to socket2 in the same way as Socket.
    // Real implementations exist in phases_early.rs for Socket; ServerSocket is less critical.
    r.register(ss, "implAccept", "(Ljava/net/Socket;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let target_socket = match args.get(1) {
            Some(Value::Object(Some(s))) => *s,
            _ => {
                return Err(RuntimeError::IOException {
                    message: "implAccept: null socket".into(),
                }
                .into())
            }
        };
        p72_impl_accept(ctx, this, target_socket)
    });

    // Socket extras — add methods not registered in phase 53
    let sock = "java/net/Socket";
    r.register(sock, "<init>", "(Ljava/net/InetAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args.get(1).copied().unwrap_or(Value::Object(None)));
        let port = match args.get(2) {
            Some(Value::Int(i)) => *i,
            _ => 0,
        };
        ctx.set_field(this, 1, Value::Int(port));
        ctx.set_field(this, 2, Value::Int(0));
        ctx.set_field(this, 3, Value::Int(0));
        Ok(None)
    });
    // Socket.bind(SocketAddress) — store the local address.
    // SocketAddress (InetSocketAddress) field layout: 0=host String, 1=port Int.
    r.register(sock, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Some(Value::Object(Some(sa))) = args.get(1) {
            // Store local port in field 2 (localPort)
            let port = ctx.get_field(*sa, 1).as_int().unwrap_or(0);
            ctx.set_field(this, 2, Value::Int(port));
        }
        Ok(None)
    });
    // Socket.connect(SocketAddress) — extract host/port, connect via std::net::TcpStream.
    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (host, port) = match args.get(1) {
                Some(Value::Object(Some(sa))) => {
                    let host = match ctx.get_field(*sa, 0) {
                        Value::Object(Some(s)) => {
                            ctx.read_string(s).unwrap_or_else(|| "127.0.0.1".into())
                        }
                        _ => "127.0.0.1".into(),
                    };
                    let port = ctx.get_field(*sa, 1).as_int().unwrap_or(0);
                    (host, port)
                }
                _ => {
                    return Err(RuntimeError::IOException {
                        message: "Socket.connect: null address".into(),
                    }
                    .into())
                }
            };
            // Connect and register in s2_registry
            use crate::servlet::{s2_alloc_stream, s2_registry};
            let addr = format!("{}:{}", host, port);
            match std::net::TcpStream::connect(&addr) {
                Ok(stream) => {
                    let stream_id = s2_alloc_stream(stream);
                    // Store host, port, stream_id in Socket fields
                    let host_s = ctx.create_string(&host);
                    ctx.set_field(this, 0, Value::Object(Some(host_s)));
                    ctx.set_field(this, 1, Value::Int(port));
                    ctx.set_field(this, 3, Value::Int(0)); // not closed
                    ctx.set_field(this, 4, Value::Int(stream_id));
                    Ok(None)
                }
                Err(e) => Err(RuntimeError::IOException {
                    message: format!("Socket.connect failed: {}", e),
                }
                .into()),
            }
        },
    );
    // Socket.connect(SocketAddress, int timeout)
    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let (host, port) = match args.get(1) {
                Some(Value::Object(Some(sa))) => {
                    let host = match ctx.get_field(*sa, 0) {
                        Value::Object(Some(s)) => {
                            ctx.read_string(s).unwrap_or_else(|| "127.0.0.1".into())
                        }
                        _ => "127.0.0.1".into(),
                    };
                    let port = ctx.get_field(*sa, 1).as_int().unwrap_or(0);
                    (host, port)
                }
                _ => {
                    return Err(RuntimeError::IOException {
                        message: "Socket.connect: null address".into(),
                    }
                    .into())
                }
            };
            let timeout_ms = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
            use crate::servlet::{s2_alloc_stream, s2_registry};
            let addr = format!("{}:{}", host, port);
            let result = if timeout_ms > 0 {
                // Resolve address and use connect_timeout
                match std::net::ToSocketAddrs::to_socket_addrs(&addr) {
                    Ok(mut iter) => match iter.next() {
                        Some(sock_addr) => std::net::TcpStream::connect_timeout(
                            &sock_addr,
                            std::time::Duration::from_millis(timeout_ms.max(0) as u64),
                        ),
                        None => Err(std::io::Error::new(
                            std::io::ErrorKind::AddrNotAvailable,
                            "no address",
                        )),
                    },
                    Err(e) => Err(e),
                }
            } else {
                std::net::TcpStream::connect(&addr)
            };
            match result {
                Ok(stream) => {
                    let stream_id = s2_alloc_stream(stream);
                    let host_s = ctx.create_string(&host);
                    ctx.set_field(this, 0, Value::Object(Some(host_s)));
                    ctx.set_field(this, 1, Value::Int(port));
                    ctx.set_field(this, 3, Value::Int(0));
                    ctx.set_field(this, 4, Value::Int(stream_id));
                    Ok(None)
                }
                Err(e) => Err(RuntimeError::IOException {
                    message: format!("Socket.connect failed: {}", e),
                }
                .into()),
            }
        },
    );
    // getLocalAddress / getLocalSocketAddress / getRemoteSocketAddress all
    // returned a constant null, and — because phase 72 runs after both
    // `phases_early::register_phase53_socket_stubs` and
    // `net_phase_e::register_re1_socket` — they REPLACED working
    // implementations registered on the same keys. Everything that logs, keys a
    // connection pool on, or rate-limits by peer address saw null for every
    // connected socket. Answer from the live stream this module already tracks
    // in slot 4; null now means "not connected", which is what the JDK returns
    // for `getRemoteSocketAddress` on an unconnected socket.
    r.register(
        sock,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match p72_socket_stream_addr(ctx, this, true) {
                Some((ip, _)) => Ok(Some(Value::Object(Some(
                    crate::net_phase_e::alloc_inet_address_unnamed(ctx, &ip)?,
                )))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(
        sock,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match p72_socket_stream_addr(ctx, this, true) {
                Some((ip, port)) => Ok(Some(Value::Object(Some(p72_alloc_inet_socket_address(
                    ctx, &ip, port,
                )?)))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    r.register(
        sock,
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match p72_socket_stream_addr(ctx, this, false) {
                Some((ip, port)) => Ok(Some(Value::Object(Some(p72_alloc_inet_socket_address(
                    ctx, &ip, port,
                )?)))),
                None => Ok(Some(Value::Object(None))),
            }
        },
    );
    let socket_adaptor_inet = "sun/nio/ch/SocketAdaptor";
    r.register(
        socket_adaptor_inet,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| p72_socket_adaptor_address(ctx, args, false),
    );
    r.register(
        socket_adaptor_inet,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| p72_socket_adaptor_address(ctx, args, true),
    );
    // isInputShutdown / isOutputShutdown answered a constant `false` even
    // straight after `shutdownInput()`/`shutdownOutput()` below had really
    // shut the stream down — the exact "half-closed socket reports itself
    // open" shape that `net_phase_e` documents as a flaky-connection-closed
    // bug. (Those two entries also replaced re1's side-table-backed getters,
    // because phase 72 registers later.) Track the shutdowns this module
    // performs, keyed by the s2 stream id, and OR in re1's side table so a
    // socket shut down through the other path still answers correctly.
    r.register(sock, "isInputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        let local = p72_shutdown_state(sid).0;
        let side = crate::net_phase_e::sock_get(ctx, this).input_shutdown != 0;
        Ok(Some(Value::Int(if local || side { 1 } else { 0 })))
    });
    r.register(sock, "isOutputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        let local = p72_shutdown_state(sid).1;
        let side = crate::net_phase_e::sock_get(ctx, this).output_shutdown != 0;
        Ok(Some(Value::Int(if local || side { 1 } else { 0 })))
    });
    // isBound() claimed every Socket was bound, including one that had never
    // been connected or bound — so `if (!s.isBound()) s.bind(...)` never ran
    // and callers that gate on it took the wrong branch. A Socket is bound once
    // it has a live stream (slot 4, set by connect/implAccept) or an explicit
    // local port from `bind` (slot 2).
    r.register(sock, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        let local_port = ctx.get_field(this, 2).as_int().unwrap_or(0);
        let bound = stream_id >= 0 || local_port != 0;
        Ok(Some(Value::Int(if bound { 1 } else { 0 })))
    });
    // Socket options (setReuseAddress, setSoLinger, set/getReceiveBufferSize,
    // set/getSendBufferSize, getTcpNoDelay, getKeepAlive, getInputStream, getOutputStream)
    // are registered with REAL implementations in phases_early.rs — not re-registered here.

    // shutdownInput/shutdownOutput — use real TcpStream::shutdown, and record
    // the half-close so `isInputShutdown`/`isOutputShutdown` above can report
    // it (the 5-slot Socket layout has no slot for the two flags).
    r.register(sock, "shutdownInput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        use crate::servlet::s2_registry;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1); // SOCK_STREAM_ID
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                let _ = stream.shutdown(std::net::Shutdown::Read);
            }
        }
        p72_mark_shutdown(stream_id, true);
        Ok(None)
    });
    r.register(sock, "shutdownOutput", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        use crate::servlet::s2_registry;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                let _ = stream.shutdown(std::net::Shutdown::Write);
            }
        }
        p72_mark_shutdown(stream_id, false);
        Ok(None)
    });
    // Socket option setters — apply to underlying stream via socket2 where possible.
    // OOBInline, performance preferences, traffic class: field-tracking via socket2::SockRef if fd exists.
    r.register(sock, "setOOBInline", "(Z)V", |ctx, args| {
        use crate::servlet::s2_registry;
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                let sock_ref = socket2::SockRef::from(stream);
                let _ = sock_ref.set_out_of_band_inline(on);
            }
        }
        Ok(None)
    });
    r.register(sock, "getOOBInline", "()Z", |ctx, args| {
        use crate::servlet::s2_registry;
        let this = obj_arg(args, 0)?;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                let sock_ref = socket2::SockRef::from(stream);
                let on = sock_ref.out_of_band_inline().unwrap_or(false);
                return Ok(Some(Value::Int(if on { 1 } else { 0 })));
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.register(sock, "sendUrgentData", "(I)V", |ctx, args| {
        // Send an OOB byte via the underlying stream.
        use crate::servlet::s2_registry;
        let this = obj_arg(args, 0)?;
        let byte = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                // socket2 doesn't expose send_oob; use regular write as best effort.
                use std::io::Write;
                let mut stream_clone =
                    stream.try_clone().map_err(|e| RuntimeError::IOException {
                        message: e.to_string(),
                    })?;
                let _ = stream_clone.write_all(&[byte]);
            }
        }
        Ok(None)
    });
    // KEEP: the no-op IS the spec. `java.net.Socket.setPerformancePreferences`
    // has an empty body in OpenJDK itself ("Not implemented yet" since 1.5) and
    // there is no getter anywhere in the JDK that could read the hints back, so
    // nothing can observe the difference between this and HotSpot.
    r.register(
        sock,
        "setPerformancePreferences",
        "(III)V",
        |_ctx, _args| Ok(None),
    );
    r.register(sock, "setTrafficClass", "(I)V", |ctx, args| {
        use crate::servlet::s2_registry;
        let this = obj_arg(args, 0)?;
        let tc = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                let sock_ref = socket2::SockRef::from(stream);
                // Map Java traffic class to IP_TOS. Only supported on IPv4.
                let _ = sock_ref.set_tos(tc as u32);
            }
        }
        Ok(None)
    });
    r.register(sock, "getTrafficClass", "()I", |ctx, args| {
        use crate::servlet::s2_registry;
        let this = obj_arg(args, 0)?;
        let stream_id = ctx.get_field(this, 4).as_int().unwrap_or(-1);
        if stream_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&stream_id) {
                let sock_ref = socket2::SockRef::from(stream);
                let tos = sock_ref.tos().unwrap_or(0);
                return Ok(Some(Value::Int(tos as i32)));
            }
        }
        Ok(Some(Value::Int(0)))
    });
    r.set_category(__prev_cat);
}

/// Both class names a `createContext` result can carry.
///
/// `com.sun.net.httpserver.HttpContext` is the ABSTRACT public type and is what
/// this VM has always minted; `sun.net.httpserver.HttpContextImpl` is what
/// HotSpot returns. Rows go on both so the carrier keeps working whichever name
/// it is minted under.
pub(crate) const HTTP_CONTEXT_CARRIERS: [&str; 2] = [
    "com/sun/net/httpserver/HttpContext",
    "sun/net/httpserver/HttpContextImpl",
];

/// The `HttpContext` accessor surface, registered in EVERY mode.
///
/// This block used to live inside `register_p72_http_server`, which is reached
/// only from `register_synthetic_overrides` -- so it existed under
/// `--synthetic-jdk` and NOWHERE ELSE. Meanwhile `createContext` mints the
/// carrier in every mode, and native dispatch is what gives an abstract carrier
/// its behaviour: with no rows registered, every accessor resolved to the
/// abstract declaration instead. Measured on 2026-09-02, compatible AND
/// `--jdk-only`:
///
/// ```text
/// getPath ! java.lang.AbstractMethodError:
///     method com/sun/net/httpserver/HttpContext.getPath()Ljava/lang/String;
///     has no Code attribute
/// ```
///
/// ...and the same for `getServer`, `getHandler`, `getAttributes`. HotSpot
/// answers all four. So `HttpServer.createContext(path, handler)` returned an
/// object on which every documented method threw, in the two arms that ship.
///
/// Called from `register_p72_http_server` (unchanged behaviour under synthetic
/// mode, which registers last and wins) and from
/// `net_phase_e::register_re10_http_server`, which runs in all modes.
pub(crate) fn register_http_context_surface(r: &mut NativeMethodRegistry) {
    // HttpContext = 2-field (path=0, handler=1)
    for hctx in HTTP_CONTEXT_CARRIERS {
        r.register(hctx, "getPath", "()Ljava/lang/String;", |ctx, args| {
            Ok(Some(ctx.get_field(obj_arg(args, 0)?, 0)))
        });
        r.register(
            hctx,
            "getHandler",
            "()Lcom/sun/net/httpserver/HttpHandler;",
            |ctx, args| Ok(Some(ctx.get_field(obj_arg(args, 0)?, 1))),
        );
        // `setAuthenticator` is a real mutator: dropping the argument silently
        // disables authentication on the context, which is a security failure
        // rather than a missing convenience. The JDK signature RETURNS the
        // authenticator it replaced — javac emits that descriptor at every real
        // call site, so the void spelling below could never have matched one; it
        // is kept (now storing too) for in-tree callers that use it.
        r.register(
            hctx,
            "setAuthenticator",
            "(Lcom/sun/net/httpserver/Authenticator;)Lcom/sun/net/httpserver/Authenticator;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let auth = match args.get(1).copied() {
                    Some(Value::Object(o)) => o,
                    _ => None,
                };
                let previous = http_context_set_authenticator(ctx, this, auth);
                Ok(Some(Value::Object(previous)))
            },
        );
        r.register(
            hctx,
            "setAuthenticator",
            "(Lcom/sun/net/httpserver/Authenticator;)V",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let auth = match args.get(1).copied() {
                    Some(Value::Object(o)) => o,
                    _ => None,
                };
                let _ = http_context_set_authenticator(ctx, this, auth);
                Ok(None)
            },
        );
        r.register(
            hctx,
            "getAuthenticator",
            "()Lcom/sun/net/httpserver/Authenticator;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(Value::Object(http_context_authenticator(ctx, this))))
            },
        );
        // getServer() — the constant null broke the documented
        // `context.getServer().getExecutor()` idiom (and any handler that walks
        // back to its server) with an NPE. `createContext` above now records the
        // owning server, so hand that back.
        r.register(
            hctx,
            "getServer",
            "()Lcom/sun/net/httpserver/HttpServer;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(Value::Object(http_link_get(
                    ctx,
                    HTTP_LINK_CONTEXT_SERVER,
                    this,
                ))))
            },
        );
        // getAttributes() is documented to return "a mutable Map" whose contents
        // persist for the life of the context — handlers use it to share state.
        // Minting a fresh empty HashMap per call meant every `put` was written to
        // a map nobody could read back. Bind ONE map per context.
        r.register(hctx, "getAttributes", "()Ljava/util/Map;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m = http_attribute_map(ctx, HTTP_LINK_CONTEXT_ATTRS, this)?;
            Ok(Some(Value::Object(Some(m))))
        });

        // HttpExchange method registrations (getRequestMethod, getRequestURI,
        // getRequestHeaders, getResponseHeaders, getRequestBody, getResponseBody,
        // sendResponseHeaders, close) are owned by `net_phase_e::register_re10_http_server`,
        // which routes them through the live HTTP/1.1 dispatch loop. Phase 72 used
        // to override those keys with naked field accessors that returned null
        // OutputStreams and never wrote a response — that broke real-server probes.
        // We keep only the ancillary getters Phase E does not register.
    }
}
