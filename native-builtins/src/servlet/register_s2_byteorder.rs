#[allow(unused_imports)]
use super::*;

// ---- ByteOrder -------------------------------------------------------------

pub fn register_s2_byteorder(r: &mut NativeMethodRegistry) {
    let bo = "java/nio/ByteOrder";
    // The factory/static natives return the CANONICAL objects (real class
    // statics when available) — the previous fresh-synthetic-per-call
    // objects broke identity comparisons (`nativeOrder() ==
    // ByteOrder.LITTLE_ENDIAN` was always false) and, in real-JDK mode,
    // wrote an order Int into the real 1-field layout's `name` String slot
    // (residual-doc item 6).
    r.register(bo, "nativeOrder", "()Ljava/nio/ByteOrder;", |ctx, _| {
        let ord = if cfg!(target_endian = "big") { 0 } else { 1 };
        Ok(Some(Value::Object(Some(s2_byte_order_object(ctx, ord)?))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // `BIG_ENDIAN` / `LITTLE_ENDIAN` were registered here with the FIELD
    // descriptor `Ljava/nio/ByteOrder;`: no door dispatches a field-shaped key
    // and `getstatic` has no registry path
    // (`docs/feature-designs/native-dispatch-architecture.md`), so they
    // were dead in every mode. Deleted in round 14 wave 2 (lane shadow, SH4-2's
    // residue); `s2_byte_order_object` still serves `nativeOrder`.
    // Layout-aware decode shared by toString/equals: real ByteOrder keeps
    // its `name` String at field 0; the synthetic stand-in keeps an order
    // Int there. The previous int-only reads decoded EVERY real constant
    // as 0 — so `LITTLE_ENDIAN.toString()` printed "BIG_ENDIAN".
    fn s2_byte_order_ord(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
        match ctx.get_field(obj, 0) {
            Value::Int(v) => v,
            Value::Object(Some(name)) => match ctx.read_string(name).as_deref() {
                Some("LITTLE_ENDIAN") => 1,
                _ => 0,
            },
            _ => 0,
        }
    }
    r.register(bo, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Real layout: return the name String itself.
        if let Value::Object(Some(name)) = ctx.get_field(this, 0) {
            return Ok(Some(Value::Object(Some(name))));
        }
        let name = if s2_byte_order_ord(ctx, this) == 1 {
            "LITTLE_ENDIAN"
        } else {
            "BIG_ENDIAN"
        };
        let s = ctx.create_string(name);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(bo, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        if this == other {
            return Ok(Some(Value::Int(1)));
        }
        let a = s2_byte_order_ord(ctx, this);
        let b = s2_byte_order_ord(ctx, other);
        Ok(Some(Value::Int(if a == b { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
}

// ---- SocketChannel (real TcpStream) ----------------------------------------

/// Run `op` (a blocking socket syscall that touches no heap object) inside
/// the VM's GC-blocked region, with `obj` pinned across it. Returns `obj`'s
/// possibly-forwarded address and `op`'s result.
///
/// gc-common w13-d: the synthetic `SocketChannel` natives ran their connect,
/// their close-aware read/write park and the syscall after it with no blocking
/// region. A thread parked there is a cooperative mutator that never reaches a
/// safepoint, so a stop-the-world collection waited for the peer: for a
/// silent peer, forever (a blocking `read`), or for the OS connect timeout
/// (about 21 s on Windows) on an unreachable address. That is the stall
/// `ServerSocketChannel.accept` and the selector poll here were already
/// bracketed against. Once the region is open a collection may move `obj`,
/// hence the pin.
pub fn s2_blocking_with_pinned<T>(
    ctx: &mut dyn NativeContext,
    obj: ObjectRef,
    op: impl FnOnce() -> T,
) -> (ObjectRef, T) {
    let pin = ctx.pin_native_root(obj);
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let out = op();
    ctx.end_blocking_region();
    let obj = ctx.read_native_pin(pin, obj);
    ctx.unpin_native_roots(pin);
    (obj, out)
}

pub fn register_s2_socket_channel(r: &mut NativeMethodRegistry) {
    let sc = "java/nio/channels/SocketChannel";

    r.register(
        sc,
        "open",
        "()Ljava/nio/channels/SocketChannel;",
        |ctx, _| {
            let ch = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
            ctx.set_field(ch, S2SC_CONNECTED, Value::Int(0));
            ctx.set_field(ch, S2SC_OPEN, Value::Int(1));
            ctx.set_field(ch, S2SC_ADDR, Value::Object(None));
            ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(-1));
            ctx.set_field(ch, S2SC_BLOCKING, Value::Int(1));
            Ok(Some(Value::Object(Some(ch))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        sc,
        "open",
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;",
        |ctx, args| {
            let addr_val = args.first().copied().unwrap_or(Value::Object(None));
            let ch = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
            ctx.set_field(ch, S2SC_CONNECTED, Value::Int(0));
            ctx.set_field(ch, S2SC_OPEN, Value::Int(1));
            ctx.set_field(ch, S2SC_ADDR, addr_val);
            ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(-1));
            ctx.set_field(ch, S2SC_BLOCKING, Value::Int(1));
            let mut ch = ch;
            if let Value::Object(Some(addr)) = addr_val {
                if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                    let (moved, connected) = s2_blocking_with_pinned(ctx, ch, || {
                        TcpStream::connect(format!("{host}:{port}"))
                    });
                    ch = moved;
                    if let Ok(stream) = connected {
                        let id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
                        ctx.set_field(ch, S2SC_SOCK_ID, Value::Int(id));
                        ctx.set_field(ch, S2SC_CONNECTED, Value::Int(1));                    }                }            }
            Ok(Some(Value::Object(Some(ch))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(sc, "connect", "(Ljava/net/SocketAddress;)Z", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        // If address is null, fall back to stub (mark as connected, return true)
        let addr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                return Ok(Some(Value::Int(1)));
            }
        };
        ctx.set_field(this, S2SC_ADDR, Value::Object(Some(addr)));
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
            let (moved, connected) =
                s2_blocking_with_pinned(ctx, this, || TcpStream::connect(format!("{host}:{port}")));
            this = moved;
            match connected {
                Ok(stream) => {
                    if !blocking {
                        let _ = stream.set_nonblocking(true);
                    }
                    let id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
                    ctx.set_field(this, S2SC_SOCK_ID, Value::Int(id));
                    ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                    return Ok(Some(Value::Int(if blocking { 1 } else { 0 })));
                }
                Err(e) => {
                    tracing::debug!("SocketChannel.connect: {e}");
                    if blocking {
                        return Err(RuntimeError::IOException {
                            message: format!("Connection refused: {host}:{port}"),
                        }
                        .into());
                    }
                }
            }
        }
        ctx.set_field(this, S2SC_CONNECTED, Value::Int(0));
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sc, "read", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bb = obj_arg(args, 1)?;
        let sock_id = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sock_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let pos = s2_bb_pos(ctx, bb) as usize;
        let lim = s2_bb_limit(ctx, bb) as usize;
        let cap = lim.saturating_sub(pos);
        if cap == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let mut tmp = vec![0u8; cap];
        // `s2_blocking_accept` leaves the streams it registers in BLOCKING
        // mode, so a `SocketChannel.read` on one of them parks in `recv` — and
        // `close` only removes the map entry, which cannot reach a thread
        // holding an `Arc` clone of the stream. Gate on the channel's own
        // recorded mode: a non-blocking channel must keep answering 0
        // (`IOStatus.UNAVAILABLE`) immediately, which is what every
        // selector-driven reactor on this surface depends on.
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        let stream = {
            let reg = s2_registry().lock();
            reg.streams.get(&sock_id).cloned()
        };
        let read_once = |tmp: &mut [u8]| -> i32 {
            let Some(stream) = stream.as_ref() else {
                return -1;
            };
            let closed_first = blocking
                && matches!(
                    s2_wait_ready_close_aware(stream_pollreq_fd(stream), false, &|| {
                        s2_stream_still_registered(sock_id)
                    }),
                    Err(_)
                );
            if closed_first {
                // Closed from another thread while parked. -1 is this
                // surface's end-of-input answer and unwinds the caller's
                // read loop, which is the outcome the close has to produce;
                // it is a weaker answer than the
                // `AsynchronousCloseException` the real `SocketChannel`
                // path raises, and is named as such in
                // W7-53-blocking-close-family.md.
                -1i32
            } else {
                let mut stream_ref: &TcpStream = stream;
                match stream_ref.read(tmp) {
                    Ok(0) => -1i32,
                    Ok(n) => n as i32,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                    Err(_) => -1,
                }
            }
        };
        // gc-common w13-d: a blocking read parks (see `s2_blocking_with_pinned`),
        // so it runs GC-blocked with `bb` pinned; a non-blocking one returns at
        // once and stays outside.
        let (bb, n) = if blocking && stream.is_some() {
            s2_blocking_with_pinned(ctx, bb, || read_once(&mut tmp[..]))
        } else {
            (bb, read_once(&mut tmp[..]))
        };
        if n > 0 {
            if let Some(arr) = s2_bb_arr(ctx, bb) {
                for i in 0..n as usize {
                    ctx.set_array_element(arr, pos + i, Value::Int(tmp[i] as i8 as i32));
                }
            }
            ctx.set_field(bb, BB_POS, Value::Int((pos + n as usize) as i32));
        }
        Ok(Some(Value::Int(n)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sc, "write", "(Ljava/nio/ByteBuffer;)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bb = obj_arg(args, 1)?;
        let sock_id = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sock_id < 0 {
            return Err(RuntimeError::IOException {
                message: "not connected".into(),
            }
            .into());
        }
        let data = s2_bb_remaining_bytes(ctx, bb);
        if data.is_empty() {
            return Ok(Some(Value::Int(0)));
        }
        // Write twin of the read above — a blocking `send` parks behind peer
        // backpressure exactly as a `recv` parks behind peer silence, and the
        // close reaches neither.
        let blocking = ctx.get_field(this, S2SC_BLOCKING).as_int().unwrap_or(1) != 0;
        let stream = {
            let reg = s2_registry().lock();
            reg.streams.get(&sock_id).cloned()
        };
        let write_once = || -> i32 {
            let Some(stream) = stream.as_ref() else {
                return -1;
            };
            let closed_first = blocking
                && matches!(
                    s2_wait_ready_close_aware(stream_pollreq_fd(stream), true, &|| {
                        s2_stream_still_registered(sock_id)
                    }),
                    Err(_)
                );
            if closed_first {
                -1i32
            } else {
                let mut stream_ref: &TcpStream = stream;
                match stream_ref.write(&data) {
                    Ok(n) => n as i32,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                    Err(_) => -1,
                }
            }
        };
        // gc-common w13-d: GC-blocked with `bb` pinned when it can park, as
        // the read above.
        let (bb, n) = if blocking && stream.is_some() {
            s2_blocking_with_pinned(ctx, bb, write_once)
        } else {
            (bb, write_once())
        };
        if n > 0 {
            let pos = s2_bb_pos(ctx, bb);
            let lim = s2_bb_limit(ctx, bb);
            ctx.set_field(bb, BB_POS, Value::Int((pos + n).min(lim)));
        }
        Ok(Some(Value::Int(n)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            s2_registry().lock().streams.remove(&sid);
        }
        ctx.set_field(this, S2SC_CONNECTED, Value::Int(0));
        ctx.set_field(this, S2SC_OPEN, Value::Int(0));
        ctx.set_field(this, S2SC_SOCK_ID, Value::Int(-1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sc, "isConnected", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SC_CONNECTED)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sc, "isOpen", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SC_OPEN)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        sc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            ctx.set_field(this, S2SC_BLOCKING, Value::Int(blocking));
            let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
            if sid >= 0 {
                let mut reg = s2_registry().lock();
                if let Some(stream) = reg.streams.get_mut(&sid) {
                    let _ = stream.set_nonblocking(blocking == 0);                }            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(sc, "finishConnect", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, S2SC_SOCK_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
            return Ok(Some(Value::Int(1)));
        }
        if let Value::Object(Some(addr)) = ctx.get_field(this, S2SC_ADDR) {
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                let (this, connected) = s2_blocking_with_pinned(ctx, this, || {
                    TcpStream::connect(format!("{host}:{port}"))
                });
                if let Ok(stream) = connected {
                    let id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
                    ctx.set_field(this, S2SC_SOCK_ID, Value::Int(id));
                    ctx.set_field(this, S2SC_CONNECTED, Value::Int(1));
                    return Ok(Some(Value::Int(1)));                }            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        sc,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        sc,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---- ServerSocketChannel (real TcpListener) --------------------------------

pub fn register_s2_server_socket_channel(r: &mut NativeMethodRegistry) {
    let ssc = "java/nio/channels/ServerSocketChannel";

    r.register(
        ssc,
        "open",
        "()Ljava/nio/channels/ServerSocketChannel;",
        |ctx, _| {
            let ch: ObjectRef =
                try_alloc_concurrent_synthetic(ctx, "java/nio/channels/ServerSocketChannel", 5)?;
            ctx.set_field(ch, S2SSC_OPEN, Value::Int(1));
            ctx.set_field(ch, S2SSC_BOUND, Value::Int(0));
            ctx.set_field(ch, S2SSC_LISTENER_ID, Value::Int(-1));
            ctx.set_field(ch, S2SSC_PORT, Value::Int(0));
            ctx.set_field(ch, S2SSC_PENDING, Value::Int(-1));
            Ok(Some(Value::Object(Some(ch))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    for desc in &[
        "(Ljava/net/SocketAddress;)Ljava/nio/channels/ServerSocketChannel;",
        "(Ljava/net/SocketAddress;I)Ljava/nio/channels/ServerSocketChannel;",
    ] {
        r.register(ssc, "bind", desc, |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Null address → stub: mark as bound without creating a real listener
            let addr = match args.get(1) {
                Some(Value::Object(Some(a))) => *a,
                _ => {
                    ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
                    return Ok(Some(Value::Object(Some(this))));
                }
            };
            if let Some((host, port)) = s2_parse_socket_addr(ctx, addr) {
                match TcpListener::bind(format!("{host}:{port}")) {
                    Ok(listener) => {
                        let id = s2_alloc_listener_for_vm(ctx.vm_identity(), listener);
                        ctx.set_field(this, S2SSC_LISTENER_ID, Value::Int(id));
                        ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
                        ctx.set_field(this, S2SSC_PORT, Value::Int(port as i32));
                    }
                    Err(e) => {
                        return Err(RuntimeError::IOException {
                            message: format!("bind {host}:{port}: {e}"),
                        }
                        .into())
                    }
                }
            } else {
                // Unresolvable address → stub bound
                ctx.set_field(this, S2SSC_BOUND, Value::Int(1));
            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub);
    }
    r.register(
        ssc,
        "accept",
        "()Ljava/nio/channels/SocketChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let lid = ctx
                .get_field(this, S2SSC_LISTENER_ID)
                .as_int()
                .unwrap_or(-1);
            if lid < 0 {
                // Stub-bound (null address) — return a disconnected stub SocketChannel
                let sc = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
                ctx.set_field(sc, S2SC_CONNECTED, Value::Int(1));
                ctx.set_field(sc, S2SC_OPEN, Value::Int(1));
                ctx.set_field(sc, S2SC_ADDR, Value::Object(None));
                ctx.set_field(sc, S2SC_SOCK_ID, Value::Int(-1));
                ctx.set_field(sc, S2SC_BLOCKING, Value::Int(1));
                return Ok(Some(Value::Object(Some(sc))));
            }
            let pending = ctx.get_field(this, S2SSC_PENDING).as_int().unwrap_or(-1);
            let stream_id = if pending >= 0 {
                ctx.set_field(this, S2SSC_PENDING, Value::Int(-1));
                pending
            } else {
                // STW-COOPERATION: an idle acceptor parks here indefinitely.
                // Without the blocked-region bracket it is still counted as a
                // cooperative mutator that can never reach a safepoint, so a
                // concurrent STW waits on it forever — the same
                // `rounds=64 pending=1 taken=0` stall root-caused for
                // `SSLSocketInputStream.read`.
                ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
                let result = s2_blocking_accept(lid);
                ctx.end_blocking_region();
                match result {
                    Some(id) => id,
                    None => return Ok(Some(Value::Object(None))),
                }
            };
            let sc = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/SocketChannel", 5)?;
            ctx.set_field(sc, S2SC_CONNECTED, Value::Int(1));
            ctx.set_field(sc, S2SC_OPEN, Value::Int(1));
            ctx.set_field(sc, S2SC_ADDR, Value::Object(None));
            ctx.set_field(sc, S2SC_SOCK_ID, Value::Int(stream_id));
            ctx.set_field(sc, S2SC_BLOCKING, Value::Int(1));
            Ok(Some(Value::Object(Some(sc))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(ssc, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx
            .get_field(this, S2SSC_LISTENER_ID)
            .as_int()
            .unwrap_or(-1);
        if lid >= 0 {
            s2_registry().lock().listeners.remove(&lid);
        }
        ctx.set_field(this, S2SSC_OPEN, Value::Int(0));
        ctx.set_field(this, S2SSC_BOUND, Value::Int(0));
        ctx.set_field(this, S2SSC_LISTENER_ID, Value::Int(-1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(ssc, "isOpen", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, S2SSC_OPEN)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        ssc,
        "configureBlocking",
        "(Z)Ljava/nio/channels/SelectableChannel;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let blocking = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
            let lid = ctx
                .get_field(this, S2SSC_LISTENER_ID)
                .as_int()
                .unwrap_or(-1);
            if lid >= 0 {
                let reg = s2_registry().lock();
                if let Some(listener) = reg.listeners.get(&lid) {
                    let _ = listener.set_nonblocking(blocking == 0);                }            }
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ssc,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        ssc,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// ---- Channel registration & Selector ---------------------------------------

pub fn s2_register_channel(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let channel = args.first().copied().unwrap_or(Value::Object(None));
    let selector = args.get(1).copied().unwrap_or(Value::Object(None));
    let ops = args.get(2).copied().unwrap_or(Value::Int(0));
    // gc-common w21-b: the channel and the selector are rooted across the
    // key's allocation and stored through their current addresses. They were
    // stored into the new key (and the selector then pinned) at their entry
    // addresses, so a collection inside the allocation left the key's
    // `channel` / `selector` slots naming vacated objects. The key and the
    // selector stay rooted across the key array's allocation too.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let channel_h = match channel {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let sel_h = match selector {
        Value::Object(Some(o)) => Some(scope.root(o)),
        _ => None,
    };
    let key_h = {
        let fresh =
            try_alloc_concurrent_synthetic(&mut *scope, "java/nio/channels/SelectionKey", 4)?;
        scope.root(fresh)
    };
    let channel = match &channel_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => channel,
    };
    let selector = match &sel_h {
        Some(h) => Value::Object(Some(scope.get(h))),
        None => selector,
    };
    let key = scope.get(&key_h);
    scope.set_field(key, 0, channel);
    scope.set_field(key, 1, selector);
    scope.set_field(key, 2, ops);
    scope.set_field(key, 3, Value::Int(0)); // readyOps = 0
                                            // Add key to selector's key list
    if let Some(sel_h) = &sel_h {
        // GC-safety: `new_ref_array` below allocates and can trigger a
        // collection that relocates `key`/`sel` (both read again after);
        // both are rooted, and re-read after it.
        let n = scope
            .get_field(scope.get(sel_h), S2SEL_NKEYS)
            .as_int()
            .unwrap_or(0) as usize;
        let new_cap = (n + 1).max(8);
        let new_arr = scope.new_ref_array(cratonvm_types::ClassId::new(0), new_cap);
        let key = scope.get(&key_h);
        let sel = scope.get(sel_h);
        let ctx = &mut *scope;
        if let Value::Object(Some(old_arr)) = ctx.get_field(sel, S2SEL_KEYS) {
            for i in 0..n {
                let k = ctx.get_array_element(old_arr, i);
                ctx.set_array_element(new_arr, i, k);
            }
        }
        ctx.set_array_element(new_arr, n, Value::Object(Some(key)));
        ctx.set_field(sel, S2SEL_KEYS, Value::Object(Some(new_arr)));
        ctx.set_field(sel, S2SEL_NKEYS, Value::Int((n + 1) as i32));
    }
    let key = scope.get(&key_h);
    Ok(Some(Value::Object(Some(key))))
}

pub fn s2_keys_as_set(
    ctx: &mut dyn NativeContext,
    sel: ObjectRef,
    selected_only: bool,
) -> Result<Value, MethodCallFailed> {
    let n = ctx.get_field(sel, S2SEL_NKEYS).as_int().unwrap_or(0) as usize;
    // GC-safety: the set builder below allocates (a ref array, the set, the
    // backing map, and a node per `add`) and can trigger a collection that
    // relocates `sel`; pin it for the whole function.
    let sel_pin = ctx.pin_native_root(sel);
    let sel = ctx.read_native_pin(sel_pin, sel);
    let keys_v = ctx.get_field(sel, S2SEL_KEYS);
    let mut ready: Vec<ObjectRef> = Vec::new();
    if let Value::Object(Some(keys_arr)) = keys_v {
        for i in 0..n {
            if let Value::Object(Some(k)) = ctx.get_array_element(keys_arr, i) {
                let rops = ctx.get_field(k, 3).as_int().unwrap_or(0);
                if !selected_only || rops != 0 {
                    ready.push(k);
                }
            }
        }
    }
    // A REAL `java.util.HashSet`, through its own `<init>` and `add`.
    //
    // This used to write the key array to absolute slot 0 and a count to slot
    // 1 -- the MAP layout on a class whose one real instance field is `map` --
    // so a servlet container's `selector.selectedKeys().iterator()` walked a
    // `map` holding an `Object[]` and saw no keys at all. `build_real_hash_set`
    // pins every element across its own allocations.
    let set = crate::build_real_hash_set(ctx, &ready)?;
    ctx.unpin_native_roots(sel_pin);
    Ok(Value::Object(Some(set)))
}

pub fn register_s2_selector(r: &mut NativeMethodRegistry) {
    let sel = "java/nio/channels/Selector";

    r.register(sel, "open", "()Ljava/nio/channels/Selector;", |ctx, _| {
        if crate::nbflags().dbg_sel {
            eprintln!("[SEL] Selector.open()");
        }
        let s = try_alloc_concurrent_synthetic(ctx, "java/nio/channels/Selector", 3)?;
        ctx.set_field(s, S2SEL_OPEN, Value::Int(1));
        ctx.set_field(s, S2SEL_KEYS, Value::Object(None));
        ctx.set_field(s, S2SEL_NKEYS, Value::Int(0));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // select() — block indefinitely until a channel is ready or wakeup
    // is called from another thread. (NEW-3: previously hard-coded a
    // 1000ms timeout and spun; now passes -1 to `poll`, the canonical
    // "wait forever" sentinel.)
    r.register(sel, "select", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::nbflags().dbg_sel {
            let open = ctx.get_field(this, S2SEL_OPEN).as_int().unwrap_or(-1);
            let nkeys = ctx.get_field(this, S2SEL_NKEYS).as_int().unwrap_or(-1);
            eprintln!("[SEL] select() open={open} nkeys={nkeys}");
        }
        let n = s2_selector_do_poll_with_timeout(ctx, this, -1);
        Ok(Some(Value::Int(n)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // select(long timeout) — matches the JDK contract exactly:
    //   * timeout == 0  → block indefinitely (equivalent to select())
    //   * timeout  > 0  → block up to `timeout` ms
    //   * timeout  < 0  → IllegalArgumentException
    // (NEW-3: previously `select(0)` incorrectly returned immediately
    // and any positive value was clamped to 30_000 ms.)
    r.register(sel, "select", "(J)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "select: missing timeout argument".into(),
                }
                .into())
            }
        };
        if raw < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "Negative timeout".into(),
            }
            .into());
        }
        // Saturate at i32::MAX so we never overflow libc::poll's int
        // parameter. A 2_147_483_647 ms timeout is ~24 days, far beyond
        // any realistic user value.
        let t = if raw == 0 {
            -1i32 // infinite
        } else if raw > i32::MAX as i64 {
            i32::MAX
        } else {
            raw as i32
        };
        let n = s2_selector_do_poll_with_timeout(ctx, this, t);
        Ok(Some(Value::Int(n)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // selectNow() — non-blocking (timeout=0)
    r.register(sel, "selectNow", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(s2_selector_do_poll_with_timeout(
            ctx, this, 0,
        ))))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    // wakeup() — write one byte to the selector's self-connected UDP
    // socket so any thread currently blocked in `select()` returns
    // promptly. (NEW-3: previously a no-op that returned `this` without
    // doing anything, leaving blocked threads stuck until their timeout.)
    r.register(
        sel,
        "wakeup",
        "()Ljava/nio/channels/Selector;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            signal_wakeup(ctx, this);
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(sel, "isOpen", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, S2SEL_OPEN);
        if crate::nbflags().dbg_sel {
            eprintln!("[SEL] isOpen() = {:?}", v);
        }
        Ok(Some(v))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sel, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, S2SEL_OPEN, Value::Int(0));
        // NEW-3: release the per-selector wakeup channel so its socket
        // fd is returned to the OS promptly rather than lingering in
        // the global map until process exit.
        release_wakeup_channel(ctx, this);
        Ok(None)
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sel, "keys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(s2_keys_as_set(ctx, this, false)?))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sel, "selectedKeys", "()Ljava/util/Set;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(s2_keys_as_set(ctx, this, true)?))
    }, cratonvm_native_api::NativeKind::SyntheticStub);

    // SelectionKey — upgrade readyOps to field 3, add convenience predicates
    let sk = "java/nio/channels/SelectionKey";
    r.register(sk, "readyOps", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, 3)))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sk, "isReadable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 1 != 0 {
                1            } else {
                0            },
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sk, "isWritable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 4 != 0 {
                1            } else {
                0            },
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sk, "isAcceptable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 16 != 0 {
                1            } else {
                0            },
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(sk, "isConnectable", "()Z", |ctx, args| {
        Ok(Some(Value::Int(
            if ctx.get_field(obj_arg(args, 0)?, 3).as_int().unwrap_or(0) & 8 != 0 {
                1            } else {
                0            },
        )))
    }, cratonvm_native_api::NativeKind::SyntheticStub);
    r.register(
        sk,
        "interestOps",
        "(I)Ljava/nio/channels/SelectionKey;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ops = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            ctx.set_field(this, 2, Value::Int(ops));
            Ok(Some(Value::Object(Some(this))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // SelectableChannel.register override
    let sac = "java/nio/channels/SelectableChannel";
    r.register(
        sac,
        "register",
        "(Ljava/nio/channels/Selector;I)Ljava/nio/channels/SelectionKey;",
        s2_register_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        sac,
        "register",
        "(Ljava/nio/channels/Selector;ILjava/lang/Object;)Ljava/nio/channels/SelectionKey;",
        s2_register_channel, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

// =============================================================================
// Phase S.3 — Real HttpClient (TCP-backed HTTP/1.1)
//
// Replaces the p60 stubs that returned a fake 200 OK with an empty body.
//
// Layout (unchanged from p60):
//   HttpClient    = 1-field  (version: Int 1=HTTP/1.1, 2=HTTP/2)
//   HttpRequest   = 4-field  (uri=0, method=1, bodyPublisher=2, headers=3)
//   HttpResponse  = 3-field  (statusCode=0, body=1, responseHeaders=2)
//   BodyPublisher = 1-field  (body: String ObjectRef or Object(None))
//
// NOTE: HTTPS is supported via native-tls for TLS connections.
// =============================================================================

/// HttpRequest field indices
pub const HR_URI: usize = 0;
pub const HR_METHOD: usize = 1;
pub const HR_BODY: usize = 2;
// field 3 = extra headers map (unused by our impl)

/// `HttpRequest.Builder.POST(publisher)` / `PUT(publisher)`: method String in
/// slot 1, body publisher in slot 2, returns the builder.
///
/// gc-common w19-d: the builder and the publisher argument are rooted across
/// the method String's allocation. Both natives stored the String and the
/// publisher (read from `args`, a copy no collection updates) through the
/// builder's entry address and returned that address.
pub fn s3_builder_set_method_and_body(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let body = match args.get(1) {
        Some(Value::Object(Some(b))) => Some(*b),
        _ => None,
    };
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let body_h = body.map(|b| scope.root(b));
    let m = scope.create_string(method);
    let this = scope.get(&this_h);
    let body_now = body_h.as_ref().map(|h| scope.get(h));
    scope.set_field(this, 1, Value::Object(Some(m)));
    scope.set_field(this, 2, Value::Object(body_now));
    Ok(Some(Value::Object(Some(this))))
}

pub fn register_s3_http_client(r: &mut NativeMethodRegistry) {
    let hc = "java/net/http/HttpClient";
    let hrb = "java/net/http/HttpRequest$Builder";

    // ---- Update builder POST/PUT to also store the body publisher at field 2 ----
    r.register(
        hrb,
        "POST",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| s3_builder_set_method_and_body(ctx, args, "POST"), cratonvm_native_api::NativeKind::SyntheticStub,
    );
    r.register(
        hrb,
        "PUT",
        "(Ljava/net/http/HttpRequest$BodyPublisher;)Ljava/net/http/HttpRequest$Builder;",
        |ctx, args| s3_builder_set_method_and_body(ctx, args, "PUT"), cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ---- Real send() ----
    r.register(
        hc,
        "send",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/net/http/HttpResponse;",
        s3_http_send, cratonvm_native_api::NativeKind::SyntheticStub,
    );

    // ---- Real sendAsync() — wraps send() ----
    r.register(
        hc,
        "sendAsync",
        "(Ljava/net/http/HttpRequest;Ljava/net/http/HttpResponse$BodyHandler;)Ljava/util/concurrent/CompletableFuture;",
        |ctx, args| {
            let resp = s3_http_send(ctx, args)?;
            let resp_val = resp.unwrap_or(Value::Object(None));
            let cf = p58_new_cf(ctx, resp_val, true)?;
            Ok(Some(Value::Object(Some(cf))))
        }, cratonvm_native_api::NativeKind::SyntheticStub,
    );
}

/// Core HTTP/1.1 send implementation.
pub fn s3_http_send(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    // ---- Parse HttpRequest ----
    // Null request → backward-compat stub 200 (preserves p60 test behaviour)
    let req_ref = match args.get(1) {
        Some(Value::Object(Some(r))) => *r,
        _ => return s3_stub_response(ctx, 200, ""),
    };

    let uri_ref = match ctx.get_field(req_ref, HR_URI) {
        Value::Object(Some(u)) => u,
        _ => return s3_stub_response(ctx, 200, ""), // null URI → stub 200
    };

    // ---- Extract URI components ----
    //
    // JDK-ONLY-LAYOUT: converted from raw slot indices to
    // `net_phase_e::uri_components`. This block used to read a private
    // `scheme=0, host=1, port=2, path=3, query=4` model — an exact duplicate of
    // the one in `http2.rs` — which on a real `java.net.URI` names `scheme`,
    // `fragment`, `authority`, `userInfo` and `host`. Only slot 0 was right,
    // and the rest failed silently: a well-typed `String` from the wrong field.
    let parts = crate::net_phase_e::uri_components(ctx, uri_ref);
    let scheme = parts
        .scheme
        .unwrap_or_else(|| "http".to_string())
        .to_lowercase();
    let host = parts.host.unwrap_or_default();
    let port_field = parts.port;
    let path = if parts.path.is_empty() {
        "/".to_string()
    } else {
        parts.path
    };
    let query = parts.query;

    if host.is_empty() {
        return s3_stub_response(ctx, 400, "Cannot determine target host from URI");
    }

    let is_https = scheme == "https";
    let default_port = if is_https { 443u16 } else { 80u16 };
    let port = if port_field > 0 {
        port_field as u16
    } else {
        default_port
    };
    let target = format!("{host}:{port}");

    // ---- Method and body ----
    let method = match ctx.get_field(req_ref, HR_METHOD) {
        Value::Object(Some(m)) => ctx.read_string(m).unwrap_or_else(|| "GET".to_string()),
        _ => "GET".to_string(),
    };
    let body_bytes: Vec<u8> = match ctx.get_field(req_ref, HR_BODY) {
        Value::Object(Some(bp)) => {
            // BodyPublisher = 1-field (string body at field 0)
            match ctx.get_field(bp, 0) {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .map(|st| st.into_bytes())
                    .unwrap_or_default(),
                _ => Vec::new(),
            }
        }
        _ => Vec::new(),
    };

    // ---- Build request line + headers ----
    let request_target = if let Some(ref q) = query {
        format!("{path}?{q}")
    } else {
        path.clone()
    };

    let mut request_str = format!(
        "{method} {request_target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nUser-Agent: CratonVM/1.0\r\nAccept: */*\r\n"
    );
    if !body_bytes.is_empty() {
        request_str.push_str(&format!(
            "Content-Length: {}\r\nContent-Type: application/octet-stream\r\n",
            body_bytes.len()
        ));
    }
    request_str.push_str("\r\n");

    // ---- Connect and perform I/O (HTTP or HTTPS) ----
    let tcp_stream = match TcpStream::connect(&target) {
        Ok(s) => s,
        Err(e) => return s3_stub_response(ctx, 0, &format!("Connection failed: {e}")),
    };

    let response_bytes: Vec<u8> = if is_https {
        // TLS handshake via native-tls
        let connector = match native_tls::TlsConnector::new() {
            Ok(c) => c,
            Err(e) => return s3_stub_response(ctx, 0, &format!("TLS init failed: {e}")),
        };
        let mut tls_stream = match connector.connect(&host, tcp_stream) {
            Ok(s) => s,
            Err(e) => return s3_stub_response(ctx, 0, &format!("TLS handshake failed: {e}")),
        };
        if tls_stream.write_all(request_str.as_bytes()).is_err() {
            return s3_stub_response(ctx, 0, "TLS write error");
        }
        if !body_bytes.is_empty() && tls_stream.write_all(&body_bytes).is_err() {
            return s3_stub_response(ctx, 0, "TLS write body error");
        }
        let mut buf = Vec::new();
        if tls_stream.read_to_end(&mut buf).is_err() {
            return s3_stub_response(ctx, 0, "TLS read error");
        }
        buf
    } else {
        // Plain HTTP
        let mut stream = tcp_stream;
        if stream.write_all(request_str.as_bytes()).is_err() {
            return s3_stub_response(ctx, 0, "Write error");
        }
        if !body_bytes.is_empty() && stream.write_all(&body_bytes).is_err() {
            return s3_stub_response(ctx, 0, "Write body error");
        }
        let mut buf = Vec::new();
        if stream.read_to_end(&mut buf).is_err() {
            return s3_stub_response(ctx, 0, "Read error");
        }
        buf
    };

    // ---- Parse status line ----
    let response_str = String::from_utf8_lossy(&response_bytes).into_owned();
    let status_code = s3_parse_status_code(&response_str);

    // ---- Extract body (after double CRLF) ----
    let body_str = if let Some(pos) = response_str.find("\r\n\r\n") {
        response_str[pos + 4..].to_string()
    } else if let Some(pos) = response_str.find("\n\n") {
        response_str[pos + 2..].to_string()
    } else {
        String::new()
    };

    // ---- Build HttpResponse synthetic ----
    s3_build_response(ctx, status_code, &body_str)
}

/// Extract HTTP status code from the first line of a response.
pub fn s3_parse_status_code(response: &str) -> i32 {
    // First line: "HTTP/1.1 200 OK"
    let first_line = response.lines().next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    parts.next(); // skip "HTTP/1.1"
    parts
        .next()
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(0)
}

/// Create a stub HttpResponse (for error/unsupported cases).
pub fn s3_stub_response(ctx: &mut dyn NativeContext, status: i32, msg: &str) -> MethodCallResult {
    s3_build_response(ctx, status, msg)
}

/// The synthetic `HttpResponse`: `[statusCode, body, headers (null: not
/// parsed)]`.
///
/// gc-common w20-e: `s3_http_send` and `s3_stub_response` allocated the
/// response first and then stored the body `String` into its address from
/// before the String's allocation. The String is now built first and rooted
/// across the response's allocation, which is the last GC point.
pub fn s3_build_response(ctx: &mut dyn NativeContext, status: i32, body: &str) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let body_ref = scope.create_string(body);
    let body_h = scope.root(body_ref);
    let response = try_alloc_concurrent_synthetic(&mut *scope, "java/net/http/HttpResponse", 3)?;
    let body_ref: ObjectRef = scope.get(&body_h);
    scope.set_field(response, 0, Value::Int(status));
    scope.set_field(response, 1, Value::Object(Some(body_ref)));
    scope.set_field(response, 2, Value::Object(None));
    Ok(Some(Value::Object(Some(response))))
}

