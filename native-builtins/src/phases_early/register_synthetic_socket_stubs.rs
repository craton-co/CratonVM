#[allow(unused_imports)]
use super::*;

/// The synthetic `java.net.Socket` / `ServerSocket` surface, split out of
/// [`register_phase53_socket_stubs`] so it can be named.
///
/// Production behaviour is unchanged — the caller above still skips this
/// entirely under the default `real_net_sockets`. It is public for the same
/// reason [`crate::util_concurrent_ext::register_synthetic_aqs_natives`] is:
/// `cratonvm-vm`'s inline tests drive hand-built receivers with no real
/// `java.net` bytecode behind them, so they cannot exercise the real path, and
/// with the synthetic path unregistered they were asserting against natives
/// nothing would ever provide.
pub fn register_synthetic_socket_stubs(r: &mut NativeMethodRegistry) {
    use crate::servlet::{s2_alloc_listener_for_vm, s2_alloc_stream_for_vm, s2_registry};
    use std::io::{Read as StdRead, Write as StdWrite};
    use std::net::TcpListener;
    use std::net::TcpStream;
        
    // ===== java.net.Socket — 5-field (host, port, localPort, closed, stream_id) =====
    let sock = "java/net/Socket";

    // Default constructor — unconnected socket
    r.register(sock, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // gc-common w20-b: `this` was held raw across the empty host.
        let this: ObjectRef = alloc_string_into_field(ctx, this, SOCK_HOST, "");
        ctx.set_field(this, SOCK_PORT, Value::Int(0));
        ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(0));
        ctx.set_field(this, SOCK_CLOSED, Value::Int(0));
        ctx.set_field(this, SOCK_STREAM_ID, Value::Int(-1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // Constructor with host + port — real TCP connect
    r.register(sock, "<init>", "(Ljava/lang/String;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let host_ref = obj_arg(args, 1)?;
        let port = args[2].as_int().unwrap_or(0);
        let host_str = ctx.read_string(host_ref).unwrap_or_default();
        ctx.set_field(this, SOCK_HOST, Value::Object(Some(host_ref)));
        ctx.set_field(this, SOCK_PORT, Value::Int(port));
        ctx.set_field(this, SOCK_CLOSED, Value::Int(0));
        ctx.set_field(this, SOCK_STREAM_ID, Value::Int(-1));

        match TcpStream::connect(format!("{host_str}:{port}")) {
            Ok(stream) => {
                let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                let id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
                ctx.set_field(this, SOCK_STREAM_ID, Value::Int(id));
                ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(local_port));
            }
            Err(e) => {
                return Err(RuntimeError::IOException {
                    message: format!("Connection refused: {host_str}:{port}: {e}"),                }
                .into());            }        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // connect(SocketAddress) — for sockets created with default constructor
    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = obj_arg(args, 1)?;
            let host_str = match ctx.get_field(addr, 0) {
                Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_default(),
                _ => String::new(),
            };
            let port = ctx.get_field(addr, 1).as_int().unwrap_or(0);
            // gc-common w20-b: `this` was held raw across the host string.
            let this: ObjectRef = alloc_string_into_field(ctx, this, SOCK_HOST, &host_str);
            ctx.set_field(this, SOCK_PORT, Value::Int(port));

            match TcpStream::connect(format!("{host_str}:{port}")) {
                Ok(stream) => {
                    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                    let id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
                    ctx.set_field(this, SOCK_STREAM_ID, Value::Int(id));
                    ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(local_port));
                }
                Err(e) => {
                    return Err(RuntimeError::IOException {
                        message: format!("Connection refused: {host_str}:{port}: {e}"),                    }
                    .into());                }            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // connect(SocketAddress, int timeout)
    r.register(
        sock,
        "connect",
        "(Ljava/net/SocketAddress;I)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let addr = obj_arg(args, 1)?;
            let host_str = match ctx.get_field(addr, 0) {
                Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_default(),
                _ => String::new(),
            };
            let port = ctx.get_field(addr, 1).as_int().unwrap_or(0);
            // gc-common w20-b: `this` was held raw across the host string.
            let this: ObjectRef = alloc_string_into_field(ctx, this, SOCK_HOST, &host_str);
            ctx.set_field(this, SOCK_PORT, Value::Int(port));

            match TcpStream::connect(format!("{host_str}:{port}")) {
                Ok(stream) => {
                    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
                    let id = s2_alloc_stream_for_vm(ctx.vm_identity(), stream);
                    ctx.set_field(this, SOCK_STREAM_ID, Value::Int(id));
                    ctx.set_field(this, SOCK_LOCAL_PORT, Value::Int(local_port));
                }
                Err(e) => {
                    return Err(RuntimeError::IOException {
                        message: format!("Connection refused: {host_str}:{port}: {e}"),                    }
                    .into());                }            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(sock, "getPort", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, SOCK_PORT)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getLocalPort", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, SOCK_LOCAL_PORT)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "isClosed", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, SOCK_CLOSED)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(if sid >= 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            s2_registry().lock().streams.remove(&sid);
        }
        ctx.set_field(this, SOCK_CLOSED, Value::Int(1));
        ctx.set_field(this, SOCK_STREAM_ID, Value::Int(-1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(h)) = ctx.get_field(this, SOCK_HOST) {
                let host_str = ctx.read_string(h).unwrap_or_default();
                // Route through the shared allocator: it records host/IP in the
                // `net_phase_e` side table AND populates a real-JDK
                // `InetAddress$InetAddressHolder`. Writing a bare `String` into
                // instance slot 0 (the typed `holder` reference field) is what
                // poisoned real-JDK InetAddress bytecode dispatch.
                let addr =
                    crate::net_phase_e::alloc_inet_address_for_input(ctx, &host_str, &host_str)?;
                Ok(Some(Value::Object(Some(addr))))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sock,
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A `ServerSocket.accept()`- or `Socket.connect()`-produced
            // Socket never writes the raw SOCK_HOST/SOCK_PORT fields (a
            // host String there would land on real JDK's `Socket.impl`
            // slot, corrupting it) -- the real peer address is side-tabled
            // instead (see net_phase_e::SockSide). Falling straight to
            // InetSocketAddress(String,int) with the raw (null) field threw
            // an NPE for every connected/accepted Socket (e.g. MockWebServer
            // MockWebServer's `serveConnection`, breaking every test that
            // logs/uses the peer address -- RestClientIntegrationTests et
            // al). Fall back to the side table when the raw field is unset.
            let side = crate::net_phase_e::sock_get(ctx, this);
            // gc-common w20-b: the port is read before the host string is
            // allocated (it read `this` after that allocation).
            let port = match ctx.get_field(this, SOCK_PORT) {
                p @ Value::Int(v) if v != 0 => p,
                _ => Value::Int(side.port),
            };
            let host = match ctx.get_field(this, SOCK_HOST) {
                h @ Value::Object(Some(_)) => h,
                _ if !side.host.is_empty() => Value::Object(Some(ctx.create_string(&side.host))),
                _ => Value::Object(Some(ctx.create_string("0.0.0.0"))),
            };
            // `host` may be a freshly-allocated string (the `create_string`
            // arms above) with no root holding it other than this local --
            // `new_object_initialized` runs the real InetSocketAddress
            // constructor bytecode (which calls InetAddress.getByName() and
            // can allocate/GC), so pin it across that call; otherwise a GC
            // in that window can reclaim/move it out from under the pending
            // arg, surfacing as `obj_arg` seeing a null host inside the ctor.
            let host_obj = match host {
                Value::Object(Some(o)) => o,
                _ => unreachable!("host is always Object(Some(_)) from the match above"),
            };
            let host_pin = ctx.pin_native_root(host_obj);
            let host = Value::Object(Some(ctx.read_native_pin(host_pin, host_obj)));
            let result = ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[host, port],
            );
            ctx.unpin_native_roots(host_pin);
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sock,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // gc-common w20-b: the port is read before the host string is
            // allocated (it read `this` after that allocation).
            let port = ctx.get_field(this, SOCK_LOCAL_PORT);
            let host_obj = ctx.create_string("127.0.0.1");
            let host_pin = ctx.pin_native_root(host_obj);
            let host_obj = ctx.read_native_pin(host_pin, host_obj);
            let host = Value::Object(Some(host_obj));
            let result = ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[host, port],
            );
            ctx.unpin_native_roots(host_pin);
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getInputStream() → returns a SocketInputStream backed by stream_id
    r.register(
        sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
            if sid < 0 {
                return Err(RuntimeError::IOException {
                    message: "Socket is not connected".into(),
                }
                .into());
            }
            let is = try_alloc_concurrent_synthetic(ctx, "java/net/SocketInputStream", 1)?;
            ctx.set_field(is, SIO_STREAM_ID, Value::Int(sid));
            Ok(Some(Value::Object(Some(is))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getOutputStream() → returns a SocketOutputStream backed by stream_id
    r.register(
        sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
            if sid < 0 {
                return Err(RuntimeError::IOException {
                    message: "Socket is not connected".into(),
                }
                .into());
            }
            let os = try_alloc_concurrent_synthetic(ctx, "java/net/SocketOutputStream", 1)?;
            ctx.set_field(os, SIO_STREAM_ID, Value::Int(sid));
            Ok(Some(Value::Object(Some(os))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(sock, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let timeout = if millis > 0 {
                    Some(std::time::Duration::from_millis(millis as u64))                } else {
                    None                };
                let _ = stream.set_read_timeout(timeout);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                if let Ok(Some(dur)) = stream.read_timeout() {
                    return Ok(Some(Value::Int(dur.as_millis() as i32)));                }            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "setKeepAlive", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                let _ = sock.set_keepalive(on);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getKeepAlive", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                if let Ok(ka) = sock.keepalive() {
                    return Ok(Some(Value::Int(if ka { 1 } else { 0 })));                }            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "setTcpNoDelay", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let _ = stream.set_nodelay(on);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getTcpNoDelay", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                if let Ok(nd) = stream.nodelay() {
                    return Ok(Some(Value::Int(if nd { 1 } else { 0 })));                }            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "setReuseAddress", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                let _ = sock.set_reuse_address(on);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getReuseAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                if let Ok(ra) = sock.reuse_address() {
                    return Ok(Some(Value::Int(if ra { 1 } else { 0 })));                }            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "setSendBufferSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                let _ = sock.set_send_buffer_size(size);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getSendBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                if let Ok(sz) = sock.send_buffer_size() {
                    return Ok(Some(Value::Int(sz as i32)));
                }
            }
        }
        Ok(Some(Value::Int(8192)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "setReceiveBufferSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                let _ = sock.set_recv_buffer_size(size);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getReceiveBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                if let Ok(sz) = sock.recv_buffer_size() {
                    return Ok(Some(Value::Int(sz as i32)));
                }
            }
        }
        Ok(Some(Value::Int(8192)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "setSoLinger", "(ZI)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let linger_secs = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                let linger = if on {
                    Some(std::time::Duration::from_secs(linger_secs as u64))                } else {
                    None                };
                let _ = sock.set_linger(linger);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "getSoLinger", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SOCK_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let sock = socket2::SockRef::from(stream);
                if let Ok(Some(dur)) = sock.linger() {
                    return Ok(Some(Value::Int(dur.as_secs() as i32)));
                }
            }
        }
        Ok(Some(Value::Int(-1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sock, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let host = if let Value::Object(Some(h)) = ctx.get_field(this, SOCK_HOST) {
            ctx.read_string(h).unwrap_or_default()
        } else {
            String::new()
        };
        let port = ctx.get_field(this, SOCK_PORT).as_int().unwrap_or(0);
        let s = ctx.create_string(&format!("Socket[addr={host},port={port}]"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // ===== SocketInputStream — real read from TcpStream =====
    //
    // THE FOURTH SURFACE of W2-2's blocked-reader defect, converged 2026-08-12.
    //
    // All three reads below cloned the `Arc<TcpStream>` out of `s2_registry`,
    // dropped the lock, and parked in a plain blocking `read` — so a
    // `Socket.close()` on another thread could not end the park. `close()`
    // removes the registry entry and shuts the socket down, but it cannot close
    // the OS handle while this reader holds an `Arc` clone of it; on Windows no
    // `shutdown` aborts a pending blocking call at all, so the reader stayed
    // parked forever. `java.net.Socket.close()` is unconditional in JDK 25:
    // "Any thread currently blocked in an I/O operation upon this socket will
    // throw a SocketException" — *will*, not *may*.
    //
    // Fixed by calling the surface that already solved it,
    // `net_phase_e::re1_read_close_aware`, rather than by copying its loop or by
    // deleting these registrations. The two surfaces key the SAME
    // `s2_registry().streams` map with the same `sid`, so the registry re-ask
    // that ends the park is the identical question here. `re1`'s own reader is
    // registered on the DIFFERENT class name `java/net/Socket$SocketInputStream`
    // (W2-2 records the correction), so neither shadows the other and this had
    // to be fixed on its own terms.
    //
    // Deleting them was the other defensible option and was NOT taken: these are
    // the only registrations of `java/net/SocketInputStream` in the tree, its
    // methods have no bytecode to fall back to, and the configuration that
    // reaches them (`--synthetic-jdk` plus `CRATONVM_SYNTHETIC_NET_SOCKETS`,
    // since `register_phase53_socket_stubs` early-returns on the default-true
    // `io.real_net_sockets`) is one this lane cannot run. Trading an unwakeable
    // read for an `UnsatisfiedLinkError` in a mode nobody measured is not a
    // repair.
    //
    // The deadline is read back off the socket's own `SO_RCVTIMEO` rather than
    // invented or defaulted. That is not optional: the close-aware loop does not
    // issue the `read` until the socket is ready, so `SO_RCVTIMEO` can never
    // fire, and passing `None` would have turned every `setSoTimeout` reader
    // into a new unbounded park. `re1_read_close_aware` answers `TimedOut` on
    // expiry, which the existing arm below already maps to
    // `SocketTimeoutException`.
    let sis = "java/net/SocketInputStream";
    r.register(sis, "read", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let mut buf = [0u8; 1];
        let stream = {
            let reg = s2_registry().lock();
            reg.streams.get(&sid).cloned()
        };
        if let Some(stream) = stream {
            let deadline = sis_read_deadline(&stream);
            match crate::net_phase_e::re1_read_close_aware(sid, &stream, &mut buf, deadline) {
                Ok(0) => Ok(Some(Value::Int(-1))),
                Ok(_) => Ok(Some(Value::Int(buf[0] as i32))),
                // A blocking TcpStream only yields WouldBlock here when the
                // configured SO_RCVTIMEO expires; TimedOut is additionally the
                // close-aware loop's own deadline expiry. InputStream.read must
                // throw the typed Java timeout instead of returning the
                // forbidden zero-byte read (or pretending the peer closed).
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    Err(RuntimeError::SocketTimeoutException {
                        message: format!("Socket read timed out: {e}"),
                    }
                    .into())
                }
                // `Interrupted` is this family's close carrier, NOT a real
                // EINTR — `re1_read_close_aware` reissues every genuine EINTR
                // itself and its poll reports EINTR as "not ready". It arrives
                // only once the registry entry is gone, i.e. `close()` ran.
                // Answering `0` here (the old arm) would have been a zero-byte
                // read, which `InputStream.read()` may never return.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => Err(
                    crate::net_phase_e::re1_socket_exception(ctx, "Socket closed"),
                ),
                Err(_) => Ok(Some(Value::Int(-1))),
            }
        } else {
            Ok(Some(Value::Int(-1)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sis, "read", "([BII)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let mut tmp = vec![0u8; len];
        let n = {
            let stream = {
                let reg = s2_registry().lock();
                reg.streams.get(&sid).cloned()
            };
            if let Some(stream) = stream {
                let deadline = sis_read_deadline(&stream);
                match crate::net_phase_e::re1_read_close_aware(sid, &stream, &mut tmp, deadline) {
                    Ok(0) => -1i32,
                    Ok(n) => n as i32,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Err(RuntimeError::SocketTimeoutException {
                            message: format!("Socket read timed out: {e}"),
                        }
                        .into());
                    }
                    // The close carrier — see the block comment above. Was `0`,
                    // a zero-byte read `InputStream.read([BII)` may never
                    // return for a non-zero `len`.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        return Err(crate::net_phase_e::re1_socket_exception(
                            ctx,
                            "Socket closed",
                        ));
                    }
                    Err(_) => -1,
                }
            } else {
                -1
            }
        };
        if n > 0 {
            for i in 0..n as usize {
                ctx.set_array_element(arr, off + i, Value::Int(tmp[i] as i8 as i32));
            }
        }
        Ok(Some(Value::Int(n)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sis, "read", "([B)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = obj_arg(args, 1)?;
        let len = ctx.array_length(arr) as usize;
        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        let mut tmp = vec![0u8; len];
        let n = {
            let stream = {
                let reg = s2_registry().lock();
                reg.streams.get(&sid).cloned()
            };
            if let Some(stream) = stream {
                let deadline = sis_read_deadline(&stream);
                match crate::net_phase_e::re1_read_close_aware(sid, &stream, &mut tmp, deadline) {
                    Ok(0) => -1i32,
                    Ok(n) => n as i32,
                    Err(e)
                        if matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) =>
                    {
                        return Err(RuntimeError::SocketTimeoutException {
                            message: format!("Socket read timed out: {e}"),
                        }
                        .into());
                    }
                    // The close carrier — see the block comment above.
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {
                        return Err(crate::net_phase_e::re1_socket_exception(
                            ctx,
                            "Socket closed",
                        ));
                    }
                    Err(_) => -1,
                }
            } else {
                -1
            }
        };
        if n > 0 {
            for i in 0..n as usize {
                ctx.set_array_element(arr, i, Value::Int(tmp[i] as i8 as i32));
            }
        }
        Ok(Some(Value::Int(n)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sis, "available", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let reg = s2_registry().lock();
            if let Some(stream) = reg.streams.get(&sid) {
                let _ = stream.set_nonblocking(true);
                let mut buf = [0u8; 8192];
                let avail = match stream.peek(&mut buf) {
                    Ok(n) => n as i32,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => 0,
                    Err(_) => 0,
                };
                let _ = stream.set_nonblocking(false);
                return Ok(Some(Value::Int(avail)));            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // W2: this was a no-op, so a caller that only ever closed the stream (the
    // usual try-with-resources shape) never released the TcpStream: the entry
    // stayed in `s2_registry` and the OS socket stayed open for the life of
    // the VM. Closing a socket's stream closes the socket in the real JDK too,
    // so mirror `java/net/Socket.close` here. The carrier is entirely ours —
    // `java/net/SocketInputStream` is only ever allocated by
    // `Socket.getInputStream` a few hundred lines above — so nothing outside
    // this file can be surprised by the stronger semantics.
    r.register(sis, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            s2_registry().lock().streams.remove(&sid);
        }
        ctx.set_field(this, SIO_STREAM_ID, Value::Int(-1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // ===== SocketOutputStream — real write to TcpStream =====
    let sos = "java/net/SocketOutputStream";
    r.register(sos, "write", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let b = args[1].as_int().unwrap_or(0) as u8;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid < 0 {
            return Err(RuntimeError::IOException {
                message: "stream closed".into(),
            }
            .into());
        }
        let stream = {
            let reg = s2_registry().lock();
            reg.streams.get(&sid).cloned()
        };
        if let Some(stream) = stream {
            let mut stream_ref = &*stream;
            let _ = stream_ref.write_all(&[b]);        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sos, "write", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = obj_arg(args, 1)?;
        let off = args[2].as_int().unwrap_or(0) as usize;
        let len = args[3].as_int().unwrap_or(0) as usize;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid < 0 {
            return Err(RuntimeError::IOException {
                message: "stream closed".into(),
            }
            .into());
        }
        let mut data = Vec::with_capacity(len);
        for i in 0..len {
            let v = ctx.get_array_element(arr, off + i).as_int().unwrap_or(0);
            data.push(v as u8);
        }
        let stream = {
            let reg = s2_registry().lock();
            reg.streams.get(&sid).cloned()
        };
        if let Some(stream) = stream {
            let mut stream_ref = &*stream;
            let _ = stream_ref.write_all(&data);        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sos, "write", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let arr = obj_arg(args, 1)?;
        let len = ctx.array_length(arr) as usize;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid < 0 {
            return Err(RuntimeError::IOException {
                message: "stream closed".into(),
            }
            .into());
        }
        let mut data = Vec::with_capacity(len);
        for i in 0..len {
            let v = ctx.get_array_element(arr, i).as_int().unwrap_or(0);
            data.push(v as u8);
        }
        let stream = {
            let reg = s2_registry().lock();
            reg.streams.get(&sid).cloned()
        };
        if let Some(stream) = stream {
            let mut stream_ref = &*stream;
            let _ = stream_ref.write_all(&data);        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sos, "flush", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let stream = {
                let reg = s2_registry().lock();
                reg.streams.get(&sid).cloned()
            };
            if let Some(stream) = stream {
                let mut stream_ref = &*stream;
                let _ = stream_ref.flush();            }        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // W2: same leak as `SocketInputStream.close` above, plus a data-loss risk —
    // `close()` must flush first, or the last unflushed write is silently
    // dropped.
    r.register(sos, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let sid = ctx.get_field(this, SIO_STREAM_ID).as_int().unwrap_or(-1);
        if sid >= 0 {
            let stream = {
                let reg = s2_registry().lock();
                reg.streams.get(&sid).cloned()
            };
            if let Some(stream) = stream {
                let mut stream_ref = &*stream;
                let _ = stream_ref.flush();
            }
            s2_registry().lock().streams.remove(&sid);
        }
        ctx.set_field(this, SIO_STREAM_ID, Value::Int(-1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // ===== java.net.ServerSocket — 4-field (port, backlog, closed, listener_id) =====
    let ss = "java/net/ServerSocket";

    r.register(ss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, SS_PORT, Value::Int(0));
        ctx.set_field(this, SS_BACKLOG, Value::Int(50));
        ctx.set_field(this, SS_CLOSED, Value::Int(0));
        ctx.set_field(this, SS_LISTENER_ID, Value::Int(-1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // ServerSocket(int port) — real bind
    r.register(ss, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args[1].as_int().unwrap_or(0);
        ctx.set_field(this, SS_PORT, Value::Int(port));
        ctx.set_field(this, SS_BACKLOG, Value::Int(50));
        ctx.set_field(this, SS_CLOSED, Value::Int(0));

        match TcpListener::bind(format!("0.0.0.0:{port}")) {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map(|a| a.port() as i32)
                    .unwrap_or(port);
                let id = s2_alloc_listener_for_vm(ctx.vm_identity(), listener);
                ctx.set_field(this, SS_LISTENER_ID, Value::Int(id));
                ctx.set_field(this, SS_PORT, Value::Int(actual_port));
            }
            Err(e) => {
                ctx.set_field(this, SS_LISTENER_ID, Value::Int(-1));
                return Err(RuntimeError::IOException {
                    message: format!("bind 0.0.0.0:{port}: {e}"),                }
                .into());            }        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // ServerSocket(int port, int backlog)
    r.register(ss, "<init>", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = args[1].as_int().unwrap_or(0);
        let backlog = args[2].as_int().unwrap_or(50);
        ctx.set_field(this, SS_BACKLOG, Value::Int(backlog));
        ctx.set_field(this, SS_CLOSED, Value::Int(0));

        match TcpListener::bind(format!("0.0.0.0:{port}")) {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map(|a| a.port() as i32)
                    .unwrap_or(port);
                let id = s2_alloc_listener_for_vm(ctx.vm_identity(), listener);
                ctx.set_field(this, SS_LISTENER_ID, Value::Int(id));
                ctx.set_field(this, SS_PORT, Value::Int(actual_port));
            }
            Err(e) => {
                ctx.set_field(this, SS_LISTENER_ID, Value::Int(-1));
                return Err(RuntimeError::IOException {
                    message: format!("bind 0.0.0.0:{port}: {e}"),                }
                .into());            }        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // bind(SocketAddress)
    r.register(ss, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = obj_arg(args, 1)?;
        let host_str = match ctx.get_field(addr, 0) {
            Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_else(|| "0.0.0.0".into()),
            _ => "0.0.0.0".into(),
        };
        let port = ctx.get_field(addr, 1).as_int().unwrap_or(0);

        match TcpListener::bind(format!("{host_str}:{port}")) {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map(|a| a.port() as i32)
                    .unwrap_or(port);
                let id = s2_alloc_listener_for_vm(ctx.vm_identity(), listener);
                ctx.set_field(this, SS_LISTENER_ID, Value::Int(id));
                ctx.set_field(this, SS_PORT, Value::Int(actual_port));
            }
            Err(e) => {
                return Err(RuntimeError::IOException {
                    message: format!("bind {host_str}:{port}: {e}"),                }
                .into());            }        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // bind(SocketAddress, int backlog)
    r.register(ss, "bind", "(Ljava/net/SocketAddress;I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let addr = obj_arg(args, 1)?;
        let backlog = args[2].as_int().unwrap_or(50);
        ctx.set_field(this, SS_BACKLOG, Value::Int(backlog));
        let host_str = match ctx.get_field(addr, 0) {
            Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_else(|| "0.0.0.0".into()),
            _ => "0.0.0.0".into(),
        };
        let port = ctx.get_field(addr, 1).as_int().unwrap_or(0);

        match TcpListener::bind(format!("{host_str}:{port}")) {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map(|a| a.port() as i32)
                    .unwrap_or(port);
                let id = s2_alloc_listener_for_vm(ctx.vm_identity(), listener);
                ctx.set_field(this, SS_LISTENER_ID, Value::Int(id));
                ctx.set_field(this, SS_PORT, Value::Int(actual_port));
            }
            Err(e) => {
                return Err(RuntimeError::IOException {
                    message: format!("bind {host_str}:{port}: {e}"),                }
                .into());            }        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // accept() → returns a connected Socket
    r.register(ss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid < 0 {
            return Err(RuntimeError::IOException {
                message: "ServerSocket is not bound".into(),
            }
            .into());
        }
        // gc-common w19-c: read the local port NOW. It used to be read off
        // `this` after the blocking accept (a thread parked in a blocking
        // region lets a collection run) and after `re1_init_socket_locks`,
        // which runs `new Object()` and can collect -- through the address
        // `obj_arg` answered on entry.
        let local_port = ctx.get_field(this, SS_PORT);
        // STW-COOPERATION: see the matching bracket in
        // `servlet.rs`'s `ServerSocketChannel.accept` — a thread parked in a
        // blocking `accept()` must be marked blocked or a concurrent STW
        // waits for a safepoint arrival that can never happen. Interpreter
        // round i3 wave 81, lane L6: the JDK's `ServerSocket.accept` blocks in
        // the native `Net.accept`: RUNNABLE in native, as the socket families
        // wave 59 measured.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let outcome = ss_accept_with_timeout(lid);
        ctx.end_blocking_region();
        match outcome {
            SsAccept::Accepted(sid) => {
                let client = try_alloc_concurrent_synthetic(ctx, "java/net/Socket", 5)?;
                // Seed socketLock/closeLock -- this bare allocation skips real
                // `Socket.<init>`; see net_phase_e::re1_init_socket_locks doc
                // comment and jndirealmintegration-ldap-connection-npe-FIXED.md.
                let client = crate::net_phase_e::re1_init_socket_locks(ctx, client)?;
                // Get peer address from the stream
                let (peer_host, peer_port) = {
                    let reg = s2_registry().lock();
                    if let Some(stream) = reg.streams.get(&sid) {
                        let addr = stream.peer_addr().ok();
                        (
                            addr.as_ref()
                                .map(|a| a.ip().to_string())
                                .unwrap_or_default(),
                            addr.as_ref().map(|a| a.port() as i32).unwrap_or(0),
                        )
                    } else {
                        (String::new(), 0)
                    }
                };
                // The client is rooted across its host string's allocation.
                let mut scope = NativeHandleScope::new(ctx);
                let client_h = scope.root(client);
                let hn = scope.create_string(&peer_host);
                let client = scope.get(&client_h);
                scope.set_field(client, SOCK_HOST, Value::Object(Some(hn)));
                scope.set_field(client, SOCK_PORT, Value::Int(peer_port));
                scope.set_field(client, SOCK_LOCAL_PORT, local_port);
                scope.set_field(client, SOCK_CLOSED, Value::Int(0));
                scope.set_field(client, SOCK_STREAM_ID, Value::Int(sid));
                Ok(Some(Value::Object(Some(client))))
            }
            SsAccept::TimedOut => Err(RuntimeError::SocketTimeoutException {
                message: "Accept timed out".into(),
            }
            .into()),
            SsAccept::Failed => Err(RuntimeError::IOException {
                message: "accept failed".into(),
            }
            .into()),
        }
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(ss, "getLocalPort", "()I", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, SS_PORT)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "isClosed", "()Z", |ctx, args| {
        Ok(Some(ctx.get_field(obj_arg(args, 0)?, SS_CLOSED)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        Ok(Some(Value::Int(if lid >= 0 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            s2_registry().lock().listeners.remove(&lid);
            // The id can be handed out again now, so its timeout must not
            // outlive it.
            ss_accept_timeouts().lock().remove(&lid);
        }
        ctx.set_field(this, SS_CLOSED, Value::Int(1));
        ctx.set_field(this, SS_LISTENER_ID, Value::Int(-1));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `setSoTimeout` used to walk to the listener and then deliberately drop
    // the value ("real timeout applied at accept time" — nothing applied it),
    // so synthetic mode silently lost every accept timeout while
    // `net_phase_e`'s rival RE.2 accept honoured one. Store it, and let
    // `ss_accept_with_timeout` enforce it.
    r.register(ss, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid < 0 {
            // Not bound: there is no listener to carry the option, and a later
            // bind() creates a different one. `accept()` on an unbound
            // ServerSocket is an error anyway.
            return Ok(None);
        }
        let mut table = ss_accept_timeouts().lock();
        if millis > 0 {
            table.insert(lid, millis as u32);
        } else {
            // 0 is "infinite" — clear rather than store a zero deadline.
            table.remove(&lid);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Reads back what `setSoTimeout` stored and `accept()` enforces; 0 still
    // means "infinite", which is now true because it is also the absence of an
    // entry rather than a value nobody kept.
    r.register(ss, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid < 0 {
            return Ok(Some(Value::Int(0)));
        }
        let millis = ss_accept_timeouts().lock().get(&lid).copied().unwrap_or(0);
        Ok(Some(Value::Int(millis as i32)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "setReuseAddress", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get(&lid) {
                let sock = socket2::SockRef::from(listener);
                let _ = sock.set_reuse_address(on);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "getReuseAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get(&lid) {
                let sock = socket2::SockRef::from(listener);
                if let Ok(ra) = sock.reuse_address() {
                    return Ok(Some(Value::Int(if ra { 1 } else { 0 })));                }            }        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "setReceiveBufferSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as usize;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get(&lid) {
                let sock = socket2::SockRef::from(listener);
                let _ = sock.set_recv_buffer_size(size);            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "getReceiveBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(listener) = reg.listeners.get(&lid) {
                let sock = socket2::SockRef::from(listener);
                if let Ok(sz) = sock.recv_buffer_size() {
                    return Ok(Some(Value::Int(sz as i32)));
                }
            }
        }
        Ok(Some(Value::Int(8192)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let port = ctx.get_field(this, SS_PORT).as_int().unwrap_or(0);
        let s = ctx.create_string(&format!("ServerSocket[port={port}]"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- java.security.cert.CertPathValidator — 1-field synthetic (algorithm=0) ---
    let cpv = "java/security/cert/CertPathValidator";
    r.register(
        cpv,
        "getInstance",
        "(Ljava/lang/String;)Ljava/security/cert/CertPathValidator;",
        |ctx, args| {
            let algo = obj_arg(args, 0)?;
            // gc-common w20-b: `algo` was held raw across the validator's
            // allocation (a class resolution, which can run `<clinit>`).
            let algo_pin = ctx.pin_native_root(algo);
            let obj =
                try_alloc_concurrent_synthetic(ctx, "java/security/cert/CertPathValidator", 1);
            let algo = ctx.read_native_pin(algo_pin, algo);
            ctx.unpin_native_roots(algo_pin);
            let obj: ObjectRef = obj?;
            ctx.set_field(obj, 0, Value::Object(Some(algo)));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(cpv, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        cpv,
        "validate",
        "(Ljava/security/cert/CertPath;Ljava/security/cert/CertPathParameters;)Ljava/security/cert/CertPathValidatorResult;",
        |ctx, _args| {
            // For PKIX validation, return a valid PKIXCertPathValidatorResult.
            // This is a simplification: real JDK validates the chain against trust anchors.
            // We accept all chains that reach here (the TLS layer does its own validation).
            let result = try_alloc_concurrent_synthetic(ctx, "java/security/cert/PKIXCertPathValidatorResult", 1)?;
            // trust_anchor field 0 — store null (no specific anchor exposed)
            ctx.set_field(result, 0, Value::Object(None));
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.security.cert.PKIXCertPathValidatorResult — 1-field (trustAnchor=0) ---
    let cpvr = "java/security/cert/PKIXCertPathValidatorResult";
    r.register(
        cpvr,
        "getTrustAnchor",
        "()Ljava/security/cert/TrustAnchor;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- java.security.cert.CertPath — 2-field synthetic (type=0, certs=1 List) ---
    let cp = "java/security/cert/CertPath";
    r.register(cp, "getType", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(cp, "getCertificates", "()Ljava/util/List;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        match ctx.get_field(this, 1) {
            Value::Object(Some(list)) => Ok(Some(Value::Object(Some(list)))),
            _ => {
                let empty = try_alloc_concurrent_synthetic(ctx, "java/util/ArrayList", 2)?;
                // gc-common w20-b: the list was held raw across its array.
                let empty: ObjectRef = alloc_array_into_field(
                    ctx,
                    empty,
                    0,
                    cratonvm_types::ArrayElementType::Reference,
                    0,
                );
                ctx.set_field(empty, 1, Value::Int(0));
                Ok(Some(Value::Object(Some(empty))))
            }
        }
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- java.security.cert.PKIXParameters — 2-field synthetic (trustStore=0, revocationEnabled=1) ---
    let pkixp = "java/security/cert/PKIXParameters";
    r.register(
        pkixp,
        "<init>",
        "(Ljava/security/KeyStore;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ks_ref = match args.get(1) {
                Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
                _ => Value::Object(None),
            };
            ctx.set_field(this, 0, ks_ref);
            ctx.set_field(this, 1, Value::Int(1)); // revocation enabled by default
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(pkixp, "isRevocationCheckingEnabled", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(pkixp, "setRevocationEnabled", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let val = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        ctx.set_field(this, 1, Value::Int(val));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- java.security.cert.TrustAnchor — 1-field synthetic (cert=0) ---
    let ta = "java/security/cert/TrustAnchor";
    r.register(
        ta,
        "<init>",
        "(Ljava/security/cert/X509Certificate;[B)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cert = match args.get(1) {
                Some(Value::Object(Some(o))) => Value::Object(Some(*o)),
                _ => Value::Object(None),
            };
            ctx.set_field(this, 0, cert);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ta,
        "getTrustedCert",
        "()Ljava/security/cert/X509Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // --- Socket/ServerSocket exception classes ---
    //
    // Field layout (matches the broader exception synthetic shape used
    // elsewhere in native-builtins):
    //   field 0: detail message (`String` or null)
    //
    // The previous registrations for `<init>(String)` discarded the message
    // entirely, so `getMessage()` always returned null even when a Java
    // caller did `new SocketException("connection refused")`. NEW-2 fix:
    // store the message on field 0 so `getMessage()`/`toString()` round-trip.
    for cls in [
        "java/net/SocketException",
        "java/net/BindException",
        "java/net/ConnectException",
        "java/net/SocketTimeoutException",
        "java/net/NoRouteToHostException",
        "java/net/PortUnreachableException",
    ] {
        // No-arg constructor: detail message is null. Initialize the slot
        // so that downstream getField calls observe a deterministic null
        // rather than uninitialized memory. We tolerate a null `this`
        // because some test harnesses invoke the raw native with a null
        // receiver to verify the class is registered at all.
        r.register(cls, "<init>", "()V", |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.first() {
                if ctx.object_num_fields(*this) > 0 {
                    ctx.set_field(*this, 0, Value::Object(None));
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge);
        // String constructor: store the message on field 0 so getMessage()
        // can return it. Defensive about the field count because some test
        // contexts allocate these via 0-field synthetics.
        r.register(cls, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
            if let Some(Value::Object(Some(this))) = args.first() {
                if ctx.object_num_fields(*this) > 0 {
                    let msg = args.get(1).copied().unwrap_or(Value::Object(None));
                    ctx.set_field(*this, 0, msg);
                }
            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge);
        // String + cause constructor (used by SocketException family in
        // OpenJDK 17+). Field 0 holds the message; we drop the cause for
        // now (matches the existing throwable synthetic which also doesn't
        // model a cause chain).
        r.register(
            cls,
            "<init>",
            "(Ljava/lang/String;Ljava/lang/Throwable;)V",
            |ctx, args| {
                if let Some(Value::Object(Some(this))) = args.first() {
                    if ctx.object_num_fields(*this) > 0 {
                        let msg = args.get(1).copied().unwrap_or(Value::Object(None));
                        ctx.set_field(*this, 0, msg);
                    }
                }
                Ok(Some(Value::Object(None)))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(cls, "getMessage", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            let nf = ctx.object_num_fields(this);
            if nf > 0 {
                Ok(Some(ctx.get_field(this, 0)))
            } else {
                Ok(Some(Value::Object(None)))
            }
        }, cratonvm_native_api::NativeKind::Bridge);
    }
    }

// ---------------------------------------------------------------------------
// java.util.ServiceLoader — stub
// ---------------------------------------------------------------------------
pub fn register_phase53_service_loader(_r: &mut NativeMethodRegistry) {
    // synthetic-stub removed: defers to real JDK bytecode
    // java/util/ServiceLoader has real JDK bytecode. The fakes made load()
    // return a 1-field synthetic and iterator()/stream()/findFirst() ALWAYS
    // empty, so no provider was ever discovered. Real ServiceLoader bytecode
    // reads META-INF/services resources and reflectively instantiates
    // providers via the underlying Class/reflection natives.
    let _ = _r;
}

// ---------------------------------------------------------------------------
// java.lang.Record — base class for records (Java 16+)
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// Sealed classes markers (Java 17+) — just interface stubs
// ---------------------------------------------------------------------------
pub fn register_phase53_sealed(r: &mut NativeMethodRegistry) {
            // Class.isSealed() -> boolean (real impl registered earlier as native_class_is_sealed)
    r.register("java/lang/Class", "isSealed", "()Z", native_class_is_sealed, cratonvm_native_api::NativeKind::Bridge);

    // Class.getPermittedSubclasses() -> Class[]
    //
    // The one body the real-JDK `getPermittedSubclasses0` uses (i8-L2): a
    // compact, typed `Class[]` (null for a class that is not sealed, as
    // HotSpot answers), with the array pinned across each mirror. This copy
    // used to hand back an `Object[]` held raw across `get_class_mirror`,
    // with a null hole for every subclass not loaded yet.
    r.register(
        "java/lang/Class",
        "getPermittedSubclasses",
        "()[Ljava/lang/Class;",
        crate::lang_class::native_class_get_permitted_subclasses, cratonvm_native_api::NativeKind::Bridge,
    );

    // Class.isRecord() -> boolean (real impl registered earlier as native_class_is_record)
    r.register("java/lang/Class", "isRecord", "()Z", native_class_is_record, cratonvm_native_api::NativeKind::Bridge);

    // Class.getRecordComponents() -> RecordComponent[]
    // WP2.1: Layout matches `lang_misc::register_p60_record` accessors —
    //   slot 0 = name (String), slot 1 = type (Class mirror), slot 2 = declaringRecord (Class mirror)
    r.register(
        "java/lang/Class",
        "getRecordComponents",
        "()[Ljava/lang/reflect/RecordComponent;",
        crate::lang_class::native_class_get_record_components, cratonvm_native_api::NativeKind::Bridge,
    );
}

// ============================================================================
// Phase 54: Atomic classes, MethodHandle stubs, Logging completions, URI/HTTP
// ============================================================================

pub(crate) fn register_phase54_natives(registry: &mut NativeMethodRegistry) {
    register_phase54_atomics(registry);
    register_phase54_method_handle(registry);
    register_phase54_logging_extras(registry);
    register_phase54_net_extras(registry);
    register_phase54_zip_stubs(registry);
}

// ---------------------------------------------------------------------------
// java.util.concurrent.atomic — AtomicInteger, AtomicLong, AtomicBoolean,
//   AtomicReference, LongAdder, DoubleAdder
// All are 1-field synthetic: field 0 = the value
// ---------------------------------------------------------------------------
pub fn register_phase54_atomics(r: &mut NativeMethodRegistry) {
            // --- AtomicInteger (1-field: value=0 as Int) ---
    let ai = "java/util/concurrent/atomic/AtomicInteger";
    r.register(ai, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // RD.1: all reads use volatile, all RMW use CAS loop for atomicity under contention.
    r.register(ai, "get", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_volatile(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "set", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "lazySet", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "getAndSet", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_val = args[1];
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                return Ok(Some(cur));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "compareAndSet", "(II)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "compareAndExchange", "(II)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            if cur == args[1] {
                if ctx.compare_and_swap_field(this, 0, cur, args[2]) {
                    return Ok(Some(cur));                }            } else {
                return Ok(Some(cur));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "weakCompareAndSet", "(II)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "weakCompareAndSetPlain", "(II)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "weakCompareAndSetVolatile", "(II)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "getAndIncrement", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_int().unwrap_or(0);
            let new_val = Value::Int(v.wrapping_add(1));
            if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                return Ok(Some(Value::Int(v)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "getAndDecrement", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_int().unwrap_or(0);
            let new_val = Value::Int(v.wrapping_sub(1));
            if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                return Ok(Some(Value::Int(v)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "getAndAdd", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delta = args[1].as_int().unwrap_or(0);
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_int().unwrap_or(0);
            let new_val = Value::Int(v.wrapping_add(delta));
            if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                return Ok(Some(Value::Int(v)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "incrementAndGet", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_int().unwrap_or(0);
            let new_val_int = v.wrapping_add(1);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Int(new_val_int)) {
                return Ok(Some(Value::Int(new_val_int)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "decrementAndGet", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_int().unwrap_or(0);
            let new_val_int = v.wrapping_sub(1);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Int(new_val_int)) {
                return Ok(Some(Value::Int(new_val_int)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "addAndGet", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delta = args[1].as_int().unwrap_or(0);
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_int().unwrap_or(0);
            let new_val_int = v.wrapping_add(delta);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Int(new_val_int)) {
                return Ok(Some(Value::Int(new_val_int)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        ai,
        "getAndUpdate",
        "(Ljava/util/function/IntUnaryOperator;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let op = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("IntUnaryOperator is null".into()),                    }
                    .into())
                }
            };
            atomic_int_update_with_operator(ctx, this, op, false)        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ai,
        "updateAndGet",
        "(Ljava/util/function/IntUnaryOperator;)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let op = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("IntUnaryOperator is null".into()),                    }
                    .into())
                }
            };
            atomic_int_update_with_operator(ctx, this, op, true)        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(ai, "getAcquire", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_volatile(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "getOpaque", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_volatile(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "getPlain", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "setRelease", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "setOpaque", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "setPlain", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "intValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "longValue", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Long(v as i64)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "floatValue", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Float(v as f32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "doubleValue", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_int().unwrap_or(0);
        Ok(Some(Value::Double(v as f64)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ai, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_int().unwrap_or(0);
        let s = ctx.create_string(&v.to_string());
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- AtomicLong (1-field: value=0 as Long) ---
    let al = "java/util/concurrent/atomic/AtomicLong";
    // Static query used by `AtomicLong.<clinit>` to pick between a lock-free
    // 64-bit CAS path and a synchronized fallback. True on every 64-bit
    // target (all Rust tier-1 hosts qualify). See the equivalent in
    // `lib.rs::native_atomic_long_vm_supports_cs8` for the cfg rationale.
    r.register(al, "VMSupportsCS8", "()Z", |_ctx, _args| {
        let supports = cfg!(target_pointer_width = "64") || cfg!(target_feature = "cmpxchg16b");
        Ok(Some(Value::Int(if supports { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "<init>", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // RD.1: all reads use volatile, all RMW use CAS loop for atomicity under contention.
    r.register(al, "get", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_volatile(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "set", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "lazySet", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "getAndSet", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_val = args[1];
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                return Ok(Some(cur));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "compareAndSet", "(JJ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "compareAndExchange", "(JJ)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            if cur == args[1] {
                if ctx.compare_and_swap_field(this, 0, cur, args[2]) {
                    return Ok(Some(cur));                }            } else {
                return Ok(Some(cur));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "weakCompareAndSet", "(JJ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "weakCompareAndSetPlain", "(JJ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "weakCompareAndSetVolatile", "(JJ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "getAndIncrement", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(v.wrapping_add(1))) {
                return Ok(Some(Value::Long(v)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "getAndDecrement", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(v.wrapping_sub(1))) {
                return Ok(Some(Value::Long(v)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "getAndAdd", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delta = args[1].as_long().unwrap_or(0);
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(v.wrapping_add(delta))) {
                return Ok(Some(Value::Long(v)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "incrementAndGet", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            let new_val = v.wrapping_add(1);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(new_val)) {
                return Ok(Some(Value::Long(new_val)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "decrementAndGet", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            let new_val = v.wrapping_sub(1);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(new_val)) {
                return Ok(Some(Value::Long(new_val)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "addAndGet", "(J)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delta = args[1].as_long().unwrap_or(0);
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            let new_val = v.wrapping_add(delta);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(new_val)) {
                return Ok(Some(Value::Long(new_val)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        al,
        "getAndUpdate",
        "(Ljava/util/function/LongUnaryOperator;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let op = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("LongUnaryOperator is null".into()),                    }
                    .into())
                }
            };
            atomic_long_update_with_operator(ctx, this, op, false)        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        al,
        "updateAndGet",
        "(Ljava/util/function/LongUnaryOperator;)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let op = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("LongUnaryOperator is null".into()),                    }
                    .into())
                }
            };
            atomic_long_update_with_operator(ctx, this, op, true)        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(al, "intValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_long().unwrap_or(0);
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "longValue", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "floatValue", "()F", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_long().unwrap_or(0);
        Ok(Some(Value::Float(v as f32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "doubleValue", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_long().unwrap_or(0);
        Ok(Some(Value::Double(v as f64)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(al, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_long().unwrap_or(0);
        let s = ctx.create_string(&v.to_string());
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- AtomicBoolean (1-field: value=0 as Int, 0=false 1=true) ---
    let ab = "java/util/concurrent/atomic/AtomicBoolean";
    r.register(ab, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Int(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "<init>", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // RD.1: AtomicBoolean with volatile + CAS.
    r.register(ab, "get", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field_volatile(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "set", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "lazySet", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "getAndSet", "(Z)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let new_val = args[1];
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                return Ok(Some(cur));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "compareAndSet", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "weakCompareAndSet", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "weakCompareAndSetPlain", "(ZZ)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
        Ok(Some(Value::Int(if ok { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ab, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_int().unwrap_or(0);
        let s = ctx.create_string(if v != 0 { "true" } else { "false" });
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- AtomicReference<V> (1-field: value=0 as Object) ---
    let ar = "java/util/concurrent/atomic/AtomicReference";
    r.register(ar, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Object(None));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ar, "<init>", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // RD.2: AtomicReference with volatile + reference-identity CAS.
    // compare_and_swap_field compares `Value`s; for Value::Object(Some(p)) this
    // compares ObjectRef pointers which is reference identity (not .equals()).
    r.register(ar, "get", "()Ljava/lang/Object;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field_volatile(this, 0);
        if crate::nbflags().dbg_loader_trace {
            if let Value::Object(Some(o)) = v {
                let cid = ctx.class_id_of_object(o);
                let cn = ctx.class_name_of_id(cid).unwrap_or_default();
                if cn.contains("RootReference") {
                    eprintln!(
                        "[LOADER-TRACE] AtomicReference.get() this={:p} value_ptr={:p} value_class={} value_cid={}",
                        this.as_ptr(), o.as_ptr(), cn, cid.as_u32()
                    );
                }
            }
        }
        Ok(Some(v))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ar, "set", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::nbflags().dbg_loader_trace {
            if let Value::Object(Some(o)) = args[1] {
                let cid = ctx.class_id_of_object(o);
                let cn = ctx.class_name_of_id(cid).unwrap_or_default();
                if cn.contains("RootReference") {
                    eprintln!(
                        "[LOADER-TRACE] AtomicReference.set() this={:p} value_ptr={:p} value_class={} value_cid={}",
                        this.as_ptr(), o.as_ptr(), cn, cid.as_u32()
                    );
                }
            }
        }
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ar, "lazySet", "(Ljava/lang/Object;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field_volatile(this, 0, args[1]);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        ar,
        "getAndSet",
        "(Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let new_val = args[1];
            loop {
                let cur = ctx.get_field_volatile(this, 0);
                if ctx.compare_and_swap_field(this, 0, cur, new_val) {
                    return Ok(Some(cur));
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ar,
        "compareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
            if crate::nbflags().dbg_loader_trace {
                if let Value::Object(Some(o)) = args[2] {
                    let cid = ctx.class_id_of_object(o);
                    let cn = ctx.class_name_of_id(cid).unwrap_or_default();
                    if cn.contains("RootReference") {
                        let expected_desc = match args[1] {
                            Value::Object(Some(e)) => {
                                let ecid = ctx.class_id_of_object(e);
                                format!("{:p}/cid={}", e.as_ptr(), ecid.as_u32())
                            }
                            _ => "null".to_string(),
                        };
                        eprintln!(
                            "[LOADER-TRACE] AtomicReference.compareAndSet() this={:p} expected={} new_ptr={:p} new_class={} new_cid={} ok={}",
                            this.as_ptr(), expected_desc, o.as_ptr(), cn, cid.as_u32(), ok
                        );
                    }
                }
            }
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ar,
        "compareAndExchange",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            loop {
                let cur = ctx.get_field_volatile(this, 0);
                if cur == args[1] {
                    if ctx.compare_and_swap_field(this, 0, cur, args[2]) {
                        return Ok(Some(cur));                    }                } else {
                    return Ok(Some(cur));
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ar,
        "weakCompareAndSet",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ar,
        "weakCompareAndSetPlain",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ar,
        "weakCompareAndSetVolatile",
        "(Ljava/lang/Object;Ljava/lang/Object;)Z",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ok = ctx.compare_and_swap_field(this, 0, args[1], args[2]);
            Ok(Some(Value::Int(if ok { 1 } else { 0 })))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ar,
        "getAndUpdate",
        "(Ljava/util/function/UnaryOperator;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let op = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("UnaryOperator is null".into()),                    }
                    .into())
                }
            };
            atomic_reference_update_with_operator(ctx, this, op, false)        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ar,
        "updateAndGet",
        "(Ljava/util/function/UnaryOperator;)Ljava/lang/Object;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let op = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                        message: Some("UnaryOperator is null".into()),                    }
                    .into())
                }
            };
            atomic_reference_update_with_operator(ctx, this, op, true)        }, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(ar, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0);
        let text = match v {
            Value::Object(Some(o)) => ctx
                .read_string(o)
                .unwrap_or_else(|| format!("Object@{:x}", o.as_ptr() as usize)),
            Value::Object(None) => "null".to_string(),
            _ => format!("{v:?}"),
        };
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- LongAdder (1-field: sum=0 as Long) ---
    let la = "java/util/concurrent/atomic/LongAdder";
    r.register(la, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "add", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let delta = args[1].as_long().unwrap_or(0);
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(v.wrapping_add(delta))) {
                return Ok(Some(Value::Object(None)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "increment", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(v.wrapping_add(1))) {
                return Ok(Some(Value::Object(None)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "decrement", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        loop {
            let cur = ctx.get_field_volatile(this, 0);
            let v = cur.as_long().unwrap_or(0);
            if ctx.compare_and_swap_field(this, 0, cur, Value::Long(v.wrapping_sub(1))) {
                return Ok(Some(Value::Object(None)));
            }
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "sum", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "sumThenReset", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let old = ctx.get_field(this, 0);
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(old))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Long(0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "longValue", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "intValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_long().unwrap_or(0);
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(la, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_long().unwrap_or(0);
        let s = ctx.create_string(&v.to_string());
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- DoubleAdder (1-field: sum=0 as Double) ---
    let da = "java/util/concurrent/atomic/DoubleAdder";
    r.register(da, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Double(0.0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "add", "(D)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let old = ctx.get_field(this, 0).as_double().unwrap_or(0.0);
        let delta = args[1].as_double().unwrap_or(0.0);
        ctx.set_field(this, 0, Value::Double(old + delta));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "sum", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "sumThenReset", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let old = ctx.get_field(this, 0);
        ctx.set_field(this, 0, Value::Double(0.0));
        Ok(Some(old))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "reset", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 0, Value::Double(0.0));
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "doubleValue", "()D", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "longValue", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_double().unwrap_or(0.0);
        Ok(Some(Value::Long(v as i64)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "intValue", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_double().unwrap_or(0.0);
        Ok(Some(Value::Int(v as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(da, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = ctx.get_field(this, 0).as_double().unwrap_or(0.0);
        let s = ctx.create_string(&v.to_string());
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- AtomicIntegerArray (1 field, as on JDK 25: array=0; the length is
    // the backing array's, FS3-2) ---
    let aia = "java/util/concurrent/atomic/AtomicIntegerArray";
    r.register(aia, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // gc-common w20-b: `this` was held raw across the array, and a
        // negative length became a huge `usize` instead of HotSpot's
        // `NegativeArraySizeException`.
        let len = atomic_array_new_length(args[1].as_int().unwrap_or(0))?;
        alloc_array_into_field(ctx, this, 0, cratonvm_types::ArrayElementType::Int, len);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "length", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(backing_array_length(ctx, this) as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "get", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            Ok(Some(ctx.get_array_element(arr, idx)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "set", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        let val = args[2];
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            ctx.set_array_element(arr, idx, val);        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "getAndSet", "(II)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        let new_val = Value::Int(args[2].as_int().unwrap_or(0));
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let (old, _) = atomic_array_rmw(ctx, arr, idx, |_| new_val);
            Ok(Some(old))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "compareAndSet", "(III)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        let expected = args[2].as_int().unwrap_or(0);
        let update = args[3].as_int().unwrap_or(0);
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let ok = atomic_array_cas(ctx, arr, idx, Value::Int(expected), Value::Int(update));
            Ok(Some(Value::Int(i32::from(ok))))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "getAndAdd", "(II)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        let delta = args[2].as_int().unwrap_or(0);
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
                Value::Int(cur.as_int().unwrap_or(0).wrapping_add(delta))            });
            Ok(Some(old))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "incrementAndGet", "(I)I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
                Value::Int(cur.as_int().unwrap_or(0).wrapping_add(1))
            });
            Ok(Some(new_val))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(aia, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let len = backing_array_length(ctx, this);
        let mut parts = Vec::new();
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            for i in 0..len {
                parts.push(
                    ctx.get_array_element(arr, i)
                        .as_int()
                        .unwrap_or(0)
                        .to_string(),
                );
            }
        }
        let s = ctx.create_string(&format!("[{}]", parts.join(", ")));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- AtomicLongArray (1 field, as on JDK 25: array=0, FS3-2) ---
    let ala = "java/util/concurrent/atomic/AtomicLongArray";
    r.register(ala, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // gc-common w20-b: `this` was held raw across the array, and a
        // negative length became a huge `usize` instead of HotSpot's
        // `NegativeArraySizeException`.
        let len = atomic_array_new_length(args[1].as_int().unwrap_or(0))?;
        alloc_array_into_field(ctx, this, 0, cratonvm_types::ArrayElementType::Long, len);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ala, "length", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(backing_array_length(ctx, this) as i32)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ala, "get", "(I)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            Ok(Some(ctx.get_array_element(arr, idx)))
        } else {
            Ok(Some(Value::Long(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ala, "set", "(IJ)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        let val = args[2];
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            ctx.set_array_element(arr, idx, val);        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ala, "getAndAdd", "(IJ)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        let delta = args[2].as_long().unwrap_or(0);
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let (old, _) = atomic_array_rmw(ctx, arr, idx, |cur| {
                Value::Long(cur.as_long().unwrap_or(0).wrapping_add(delta))
            });
            Ok(Some(old))
        } else {
            Ok(Some(Value::Long(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ala, "incrementAndGet", "(I)J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let idx = args[1].as_int().unwrap_or(0) as usize;
        if let Value::Object(Some(arr)) = ctx.get_field(this, 0) {
            let (_, new_val) = atomic_array_rmw(ctx, arr, idx, |cur| {
                Value::Long(cur.as_long().unwrap_or(0).wrapping_add(1))
            });
            Ok(Some(new_val))
        } else {
            Ok(Some(Value::Long(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);

    register_atomic_reference_array_natives(r);
    }

/// AtomicReferenceArray natives — needed in BOTH synthetic-jdk mode (where the
/// whole of phase54_atomics is called) and real-JDK mode (where the VarHandle
/// dispatch chain used by the real JDK bytecode can't CAS array elements).
///
/// Called from `register_phase54_atomics` in synthetic mode and directly from
/// `register_essential_natives` in real-JDK mode.
/// Resolve `(backing array, RANGE-CHECKED index)` for an `AtomicReferenceArray`
/// element native.
///
/// The twin of `util_concurrent_ext`'s `atomic_array_slot`, differing only in
/// how the backing array is reached: `AtomicIntegerArray`/`AtomicLongArray`
/// read slot 0, this one reads the field by NAME so it lands on the real JDK
/// layout's `array` whatever number that layout gives it.
///
/// `Ok(None)` = no receiver / no backing array, which keeps each caller's
/// historical `null`-shaped default. `Err` = out of range, and HotSpot throws
/// `ArrayIndexOutOfBoundsException` there — see `atomic_array_index` for the
/// measured before/after table and for why the check belongs on a funnel.
/// The backing array of an `AtomicReferenceArray`, with the `array` field
/// index memoized per receiver class.
///
/// `get_field_by_name` takes the class-manager read lock and walks the
/// hierarchy comparing field-name strings on EVERY call. That is affordable
/// for a cold accessor and is not what these three are: netty's MPSC timeout
/// queue is `AtomicReferenceArray`-backed, so the exact census
/// (`--nojit CRATONVM_DISABLE_INTRINSICS=1`) puts `lazySet` at **2.00** and
/// `get` at **1.00** calls per expired task on `HashedWheelTimerTest` —
/// `offer` is one `soElement`, `poll` is one `lvElement` plus one
/// `soElement(null)`. Measured from compiled code at 183 ns and 143 ns against
/// HotSpot's 0.91 and 0.30 (`probes/FunnelRungRate.java`).
///
/// Same memo shape, and for the same reason, as
/// `AbstractOwnableSynchronizer.setExclusiveOwnerThread`'s in `jmx.rs` and
/// `time_unit_ordinal`'s in `lib.rs`. The index is a per-class constant, so it
/// is keyed on the receiver's class id; a miss falls back to the resolving
/// path, so an unexpected layout is slow rather than wrong, and the `None`
/// answer for a missing/!reference `array` field is unchanged. Class
/// redefinition needs no invalidation: it allocates a NEW `ClassId`, so a
/// stale entry can never be consulted for the redefined class.
///
/// Two slots: `AtomicReferenceArray` is one class, and the second exists only
/// for an image that subclasses it.
///
/// Keyed on (VM identity, class id): `ClassId`s are minted per VM and one
/// thread can run natives for several VMs (gc-common w18-g).
pub fn ara_backing_array(ctx: &dyn NativeContext, this: ObjectRef) -> Option<ObjectRef> {
    const MEMO_SLOTS: usize = 2;
    thread_local! {
        static ARRAY_FIELD_INDEX: std::cell::RefCell<[(usize, u32, u32); MEMO_SLOTS]> =
            const { std::cell::RefCell::new([(0, u32::MAX, 0); MEMO_SLOTS]) };
    }
    let class_id = ctx.class_id_of_object(this);
    let raw_cid = class_id.as_u32();
    let vm = ctx.vm_identity();
    let cached = ARRAY_FIELD_INDEX.with(|memo| {
        memo.borrow()
            .iter()
            .find(|(memo_vm, cid, _)| *cid == raw_cid && *memo_vm == vm)
            .map(|(_, _, index)| *index as usize)
    });
    if let Some(index) = cached {
        if let Value::Object(Some(o)) = ctx.get_field(this, index) {
            return Some(o);
        }
    }
    let index = ctx.resolve_field_index_by_class_id(class_id, "array")?;
    match ctx.get_field(this, index) {
        Value::Object(Some(o)) => {
            ARRAY_FIELD_INDEX.with(|memo| {
                let mut memo = memo.borrow_mut();
                // Take a free slot, else evict slot 0. The policy does not need
                // to be clever at this size, but the table must not be able to
                // grow without bound.
                let victim = memo
                    .iter()
                    .position(|(_, cid, _)| *cid == u32::MAX)
                    .unwrap_or(0);
                // Cast: field counts are far below `u32::MAX`.
                memo[victim] = (vm, raw_cid, index as u32);
            });
            Some(o)
        }
        _ => None,
    }
}

pub fn ara_slot(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> Result<Option<(ObjectRef, usize)>, MethodCallFailed> {
    let this = obj_arg(args, 0)?;
    let raw = atomic_array_raw_index(args);
    let arr = match ara_backing_array(ctx, this) {
        Some(o) => o,
        None => return Ok(None),
    };
    Ok(Some((arr, atomic_array_index(ctx, arr, raw)?)))
}

pub fn register_atomic_reference_array_natives(r: &mut NativeMethodRegistry) {
            // --- AtomicReferenceArray (JDK layout: single instance field `array` at index 0) ---
    let ara = "java/util/concurrent/atomic/AtomicReferenceArray";
    r.register(ara, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // `new AtomicReferenceArray(-1)` throws NegativeArraySizeException on
        // HotSpot (measured, JDK 25.0.3+9). The old `as usize` handed
        // `usize::MAX` to `new_array` instead.
        let len = atomic_array_new_length(args[1].as_int().unwrap_or(0))?;
        // gc-common w20-b: `this` was held raw across the array.
        let this_pin = ctx.pin_native_root(this);
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, len);
        let this = ctx.read_native_pin(this_pin, this);
        ctx.unpin_native_roots(this_pin);
        // Use by-name writes so we land on the real `array` slot regardless of
        // how the class layout numbers its fields.
        ctx.set_field_by_name(this, "array", Value::Object(Some(arr)));
        // Void method — return None so nothing is pushed on the operand stack.
        Ok(None)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ara, "length", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(arr)) = ctx.get_field_by_name(this, "array") {
            Ok(Some(Value::Int(ctx.array_length(arr) as i32)))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    let ara_get = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        match ara_slot(ctx, args)? {
            Some((arr, idx)) => Ok(Some(ctx.get_array_element(arr, idx))),
            None => Ok(Some(Value::Object(None))),
        }
    };
    r.register(ara, "get", "(I)Ljava/lang/Object;", ara_get, cratonvm_native_api::NativeKind::Intrinsic);
    let ara_set = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        let val = args[2];
        if let Some((arr, idx)) = ara_slot(ctx, args)? {
            ctx.set_array_element(arr, idx, val);
        }
        Ok(None)
    };
    r.register(ara, "set", "(ILjava/lang/Object;)V", ara_set, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(ara, "lazySet", "(ILjava/lang/Object;)V", ara_set, cratonvm_native_api::NativeKind::Intrinsic);
    let ara_cas = |ctx: &mut dyn NativeContext, args: &[Value]| -> MethodCallResult {
        match ara_slot(ctx, args)? {
            Some((arr, idx)) => {
                let ok = atomic_array_cas(ctx, arr, idx, args[2], args[3]);
                Ok(Some(Value::Int(i32::from(ok))))
            }
            None => Ok(Some(Value::Int(0))),
        }
    };
    r.register(
        ara,
        "compareAndSet",
        "(ILjava/lang/Object;Ljava/lang/Object;)Z",
        ara_cas, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ara,
        "weakCompareAndSet",
        "(ILjava/lang/Object;Ljava/lang/Object;)Z",
        ara_cas, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ara,
        "weakCompareAndSetPlain",
        "(ILjava/lang/Object;Ljava/lang/Object;)Z",
        ara_cas, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        ara,
        "getAndSet",
        "(ILjava/lang/Object;)Ljava/lang/Object;",
        |ctx, args| {
            let new_val = args[2];
            match ara_slot(ctx, args)? {
                Some((arr, idx)) => {
                    let (old, _) = atomic_array_rmw(ctx, arr, idx, |_| new_val);
                    Ok(Some(old))                }
                None => Ok(Some(Value::Object(None))),
            }
        }, cratonvm_native_api::NativeKind::Intrinsic,
    );

    // NOTE: `AtomicStampedReference` registrations used to live here with a
    // buggy 2-field (ref@0, stamp@1) layout that wrote stamp out-of-bounds on
    // the real 1-slot ASR object. They have been removed and replaced by the
    // JDK-faithful single-`pair`-field intrinsic in
    // `register_atomic_stamped_ref_natives` (lib.rs), which is registered later
    // (and previously already overwrote these). Keeping a single source of
    // truth avoids the registration-order fragility.
    }

// ---------------------------------------------------------------------------
// java.lang.invoke — MethodHandle, MethodType, MethodHandles (stubs)
// ---------------------------------------------------------------------------
// ---------------------------------------------------------------------------
// java.util.logging extras: Handler, LogRecord, Formatter, LogManager
// ---------------------------------------------------------------------------
/// `java.util.logging.Handler.close()` / `.flush()` (W2).
///
/// Both are abstract on the real `Handler`, so this native is reached with a
/// CONCRETE subclass as the receiver. Dispatch to that subclass's own bytecode;
/// only a bare `java.util.logging.Handler` (which has no implementation
/// anywhere) keeps the historical no-op.
/// Does this `Handler` receiver carry the REAL JDK field layout?
///
/// MEASURED, `javap -p --system` on Temurin 25.0.3+9 — `java.util.logging.Handler`
/// declares six INSTANCE fields, in this order:
///
/// ```text
///   0 manager(LogManager)  1 filter  2 formatter
///   3 logLevel(Level)      4 errorManager        5 encoding
/// ```
///
/// (`offValue` is static and takes no slot.) So slot 0 is the `LogManager`, and
/// a native reading slot 0 for the level answers the log manager while one
/// WRITING slot 0 overwrites it — which is exactly what `getLevel`/`setLevel`
/// did in `--real-jdk` until 2026-08-24. `probes/JulHandlerLevel.java` measures
/// both halves off the real fields through `--add-opens`:
///
/// ```text
///   --real-jdk   getLevel() on a fresh Handler -> java.util.logging.LogManager@587
///                setLevel(WARNING) -> Handler.logLevel still ALL,
///                                     Handler.manager now holds WARNING
///   compatible   correct        HotSpot 25.0.3+9   correct
/// ```
///
/// The count is the discriminator rather than a name probe because every
/// instance field here is a REFERENCE, so `get_field_by_name` cannot be
/// type-checked the way [`log_record_real_layout`](crate::log_record_real_layout)
/// checks `longThreadID` for a `Long` — and it answers `Value::Object(None)` for
/// a name it cannot resolve, which is indistinguishable from a genuinely null
/// `logLevel` ("inherit from the parent" is a legal state). Same shape as
/// `panama_libffi::segment_address`'s `object_num_fields(seg) >= 6`.
pub fn handler_real_layout(ctx: &dyn NativeContext, h: cratonvm_types::ObjectRef) -> bool {
    ctx.object_num_fields(h) >= 6
}

pub fn jul_handler_delegate(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    method: &'static str,
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let cid = ctx.class_id_of_object(this);
    let is_bare_handler = match ctx.class_name_of_id(cid) {
        Some(name) => name == "java/util/logging/Handler",
        // Unknown class: keep the old no-op rather than risk an
        // AbstractMethodError on a receiver we cannot identify.
        None => true,
    };
    if is_bare_handler {
        return Ok(None);
    }
    // `_bytecode_only` skips native lookup, so this cannot re-enter here.
    ctx.invoke_virtual_bytecode_only(this, method, "()V", &[])?;
    Ok(None)
}

pub fn register_phase54_logging_extras(r: &mut NativeMethodRegistry) {
            // --- LogRecord ---
    // Slots mirror the small accessor surface WildFly/JBoss copies during logging
    // bootstrap: level, message, source, logger, resource bundle, sequence, etc.
    // Real-JDK-layout records (crate::log_record_real_layout) resolve every
    // field by NAME instead: the synthetic slots disagree with the real JDK
    // declaration order almost everywhere (synthetic slot 4 "loggerName" is
    // the real layout's `message`, slot 5 "millis" is the real `threadID`,
    // ...), so raw-slot access on a real record reads or clobbers unrelated
    // fields -- the source of Tomcat JULI's garbled file-log lines.
    fn lr_get(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        name: &'static str,
        slot: usize,
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if crate::log_record_real_layout(ctx, this) {
            Ok(Some(ctx.get_field_by_name(this, name)))
        } else {
            Ok(Some(ctx.get_field(this, slot)))
        }
    }
    fn lr_set(
        ctx: &mut dyn NativeContext,
        args: &[Value],
        name: &'static str,
        slot: usize,
    ) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let value = args.get(1).copied().unwrap_or(Value::Object(None));
        if crate::log_record_real_layout(ctx, this) {
            ctx.set_field_by_name(this, name, value);
        } else {
            ctx.set_field(this, slot, value);
        }
        Ok(Some(Value::Object(None)))
    }
    let lr = "java/util/logging/LogRecord";
    // NOT `Intrinsic`, for the same reason as the source-pair block below, and
    // this is the row that actually held W7-56 open. An intrinsic cannot give
    // an answer the bytecode would not; this constructor gives a DIFFERENT
    // object. JDK 25's `LogRecord(Level, String)` ends with
    // `needToInferCaller = true` and assigns `sequenceNumber` from
    //
    // `needToInferCaller` is the whole defect. Measured under `--real-jdk` with
    // `--add-opens=java.logging/java.util.logging=ALL-UNNAMED`, on a binary
    // where the four source accessors were ALREADY retired and the REAL lazy
    // getter was running:
    //
    //   HotSpot     A4 fresh needToInferCaller=true
    //   CratonVM    A4 fresh needToInferCaller=false
    //
    // A real getter reading a false flag never calls `inferCaller()`, so the
    // source pair stays null no matter how well the walk works. Retiring the
    // getters alone was necessary and not sufficient.
    //
    // `java/util/logging/LogRecord.<init>(Level,String)` has been in
    // `RETIRED_SHADOW_TRIPLES` since the 2026-08-11 wave, but the retag arm in
    // `NativeMethodRegistry::register` only fires on an effective category of
    // `Bridge`, and this function's ambient category is `Intrinsic` — so the
    // entry has been INERT the whole time. Under `--real-jdk` the OTHER
    // registration of this triple (native-builtins/src/lib.rs, `Bridge`) is
    // registration of this triple (native-builtins/src/lib.rs, `Bridge`) OWNS
    // the slot -- MEASURED 2026-08-13 with `--dump-native-registry`:
    // lib.rs owns=true inv=2, this one owns=false inv=0. The claim that ran
    // the other way round sent a later fix into THIS dead body, where it
    // changed nothing. Put LogRecord ctor changes in lib.rs.
    // Fourth instance of the ambient-category defect on this file's JUL rows.
    // W7-56-infercaller-strict-RETIRED-20260917.md
    //
    // Spelled as an explicit set/restore rather than `with_category`, matching
    // this file's own idiom at line 101 and elsewhere: a closure would re-indent
    // ninety lines of body for a one-line category change, and the tree is not
    // `cargo fmt`-clean, so that diff would bury the change that matters.
            r.register(
        lr,
        "<init>",
        "(Ljava/util/logging/Level;Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let level = args.get(1).copied().unwrap_or(Value::Object(None));
            let msg = args.get(2).copied().unwrap_or(Value::Object(None));
            let real = crate::log_record_real_layout(ctx, this);
            if real {
                ctx.set_field_by_name(this, "level", level);
                ctx.set_field_by_name(this, "message", msg);
            } else {
                ctx.set_field(this, 0, level);
                ctx.set_field(this, 1, msg);
                ctx.set_field(this, 2, Value::Object(None));
                ctx.set_field(this, 3, Value::Object(None));
                ctx.set_field(this, 4, Value::Object(None));
                ctx.set_field(this, 6, Value::Object(None));
                ctx.set_field(this, 7, Value::Object(None));
                ctx.set_field(this, 8, Value::Object(None));
                ctx.set_field(this, 9, Value::Long(0));
                ctx.set_field(this, 11, Value::Object(None));
            }
            // Real JDK's ctor stamps the constructing thread's id into
            // threadID/longThreadID (without it Tomcat JULI's
            // OneLineFormatter feeds 0 to ThreadMXBean.getThreadInfo(long):
            // "Invalid thread ID parameter" on every AsyncFileHandler
            // format) and the creation Instant, which getMillis()/the JULI
            // timestamp column read back. The lookups below may allocate, so
            // keep `this` pinned across them.
            let this_pin = ctx.pin_native_root(this);
            let tid = crate::current_java_thread_tid(ctx);
            let now_ms = crate::epoch_millis_now();
            let instant = if real {
                ctx.invoke(
                    "java/time/Instant",
                    "ofEpochMilli",
                    "(J)Ljava/time/Instant;",
                    &[Value::Long(now_ms)],
                )
                .ok()
                .flatten()
            } else {
                None
            };
            let this = ctx.read_native_pin(this_pin, this);
            let short_tid = crate::short_thread_id(tid);
            if real {
                ctx.set_field_by_name(this, "threadID", Value::Int(short_tid));
                ctx.set_field_by_name(this, "longThreadID", Value::Long(tid));
                if let Some(instant @ Value::Object(Some(_))) = instant {
                    ctx.set_field_by_name(this, "instant", instant);
                }
            } else {
                ctx.set_field(this, 5, Value::Long(now_ms));
                ctx.set_field(this, 10, Value::Int(short_tid));
            }
            // Mirrors the real ctor's closing `needToInferCaller = true`. This
            // shadow only runs in `Compatible` now, where nothing reads the
            // flag (the source-pair getters are natives there), so it is inert
            // — but a shadow that silently drops a field the real constructor
            // sets is exactly what cost W7-56 a build, and the next reader of
            // this block should not have to rediscover that.
            if real {
                ctx.set_field_by_name(this, "needToInferCaller", Value::Int(1));
            }
            ctx.unpin_native_roots(this_pin);
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
        // `getLevel` and `getMessage` are NOT registered here -- deleted
    // 2026-09-12 with `getSequenceNumber` below and the two `Handler` rows
    // further down. All five triples are entries in `RETIRED_SHADOW_TRIPLES`,
    // and all five retirements were INERT: the ambient category of this
    // function is `Intrinsic`, `Intrinsic` is allowed under `--real-jdk`, and
    // `--real-jdk` refuses only the honest stubs on the same triples. A refusal
    // is not a removal, so the refused stub left THESE bodies in the slot and
    // the real JDK bytecode the retirement exists to reach never ran.
    //
    // The note above `Handler.setLevel` had this written down -- "the
    // `java/util/logging/` shadow retirement does not reach it either" -- and
    // W7-25 and W7-56 had each lifted other rows out of this block into
    // `Bridge` for the same reason. What was missing was an instrument, not a
    // diagnosis: `the native-registry census`'s `no_retired_triple_survives_the_strict_boot`
    // is one line of set arithmetic over the strict boot's own registry.
    //
    // DELETED rather than re-tagged `Bridge` the way W7-25 and W7-56 did,
    // for two measured reasons. First, these five are already dead in
    // compatible mode in all three feature arms -- a later registration
    // (`lib.rs`, `reflect_annotations.rs`, `logmanager.rs`, `nio_native.rs`)
    // owns every one of the slots -- so deleting changes nothing a compatible
    // run does. Second, the kind-map gate fires on a CHANGED kind and
    // explicitly tolerates a removed row (`scripts/jdk-only-kind-map.py`:
    // "removed rows pass and are reported"), so a re-tag would owe an
    // ~11,900-line baseline re-freeze that a deletion does not.
    r.register(lr, "setMessage", "(Ljava/lang/String;)V", |ctx, args| {
        lr_set(ctx, args, "message", 1)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    // NOT `Intrinsic`, unlike the ambient category this function sets at its
    // head. An intrinsic is the kind that cannot give an answer the bytecode
    // would not — and these can. `getSourceClassName` here is the REAL getter
    // with the `inferCaller()` call deleted: JDK 25's body is
    // `if (needToInferCaller) inferCaller(); return sourceClassName;`, this one
    // is a bare field read, and the field is null because nothing ever inferred
    // it. Symmetrically `setSourceClassName` is the real setter with its
    // `needToInferCaller = false` deleted. Measured: HotSpot renders the caller
    // class, CratonVM renders null.
    //
    // `Intrinsic` is exempt from the `java/util/logging/` shadow retirement, so
    // under `--real-jdk` these survived after the retirement refused every
    // honest `Bridge` and `SyntheticStub` on the same classes — a refusal is
    // not a removal. W7-25 lifted the two `LogManager` rows out of the ambient
    // block for exactly this reason; these are the same defect at sites that
    // pass did not cover. W7-35-jul-supplier-and-payload-residuals.md
    //
    // The `Bridge` tag alone did NOT retire them: it made the `--real-jdk`
    // census REPORT them (`bridge-ran-over-bytecode`) and left them dispatching,
    // which is why the source pair was still null in strict mode a day later.
    // All four triples are entries in `RETIRED_SHADOW_TRIPLES`
    // (native-api/src/retired_shadow.rs) as of 2026-08-12, so `--real-jdk`
    // refuses them and the real lazy-inference bytecode runs, while
    // `Compatible` still dispatches them over records the JUL bridge stamped at
    // construction. They retire as a SET of four: see that module's docs for
    // why a getter-only retirement is worse than none.
    // W7-56-infercaller-strict-RETIRED-20260917.md
    {
        r.register(
            lr,
            "getSourceClassName",
            "()Ljava/lang/String;",
            |ctx, args| lr_get(ctx, args, "sourceClassName", 2), cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            lr,
            "setSourceClassName",
            "(Ljava/lang/String;)V",
            |ctx, args| lr_set(ctx, args, "sourceClassName", 2), cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            lr,
            "getSourceMethodName",
            "()Ljava/lang/String;",
            |ctx, args| lr_get(ctx, args, "sourceMethodName", 3), cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            lr,
            "setSourceMethodName",
            "(Ljava/lang/String;)V",
            |ctx, args| lr_set(ctx, args, "sourceMethodName", 3), cratonvm_native_api::NativeKind::Bridge,
        );
    };

    r.register(lr, "getLoggerName", "()Ljava/lang/String;", |ctx, args| {
        lr_get(ctx, args, "loggerName", 4)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "setLoggerName", "(Ljava/lang/String;)V", |ctx, args| {
        lr_set(ctx, args, "loggerName", 4)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "getMillis", "()J", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::log_record_real_layout(ctx, this) {
            // Real bytecode derives millis from `instant`.
            if let Value::Object(Some(instant)) = ctx.get_field_by_name(this, "instant") {
                return ctx.invoke_virtual(instant, "toEpochMilli", "()J", &[]);
            }
            return Ok(Some(Value::Long(0)));
        }
        match ctx.get_field(this, 5) {
            millis @ Value::Long(_) => Ok(Some(millis)),
            _ => Ok(Some(Value::Long(0))),
        }
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "setMillis", "(J)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let millis = match args.get(1) {
            Some(Value::Long(v)) => *v,
            Some(Value::Int(v)) => *v as i64,
            _ => 0,
        };
        if crate::log_record_real_layout(ctx, this) {
            // Mirror the real setter: replace `instant`. The Instant
            // construction may allocate; keep `this` pinned across it.
            let this_pin = ctx.pin_native_root(this);
            let instant = ctx
                .invoke(
                    "java/time/Instant",
                    "ofEpochMilli",
                    "(J)Ljava/time/Instant;",
                    &[Value::Long(millis)],
                )
                .ok()
                .flatten();
            let this = ctx.read_native_pin(this_pin, this);
            if let Some(instant @ Value::Object(Some(_))) = instant {
                ctx.set_field_by_name(this, "instant", instant);            }
            ctx.unpin_native_roots(this_pin);        } else {
            ctx.set_field(this, 5, Value::Long(millis));        }
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "getParameters", "()[Ljava/lang/Object;", |ctx, args| {
        lr_get(ctx, args, "parameters", 6)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(
        lr,
        "setParameters",
        "([Ljava/lang/Object;)V",
        |ctx, args| lr_set(ctx, args, "parameters", 6), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        lr,
        "getResourceBundle",
        "()Ljava/util/ResourceBundle;",
        |ctx, args| lr_get(ctx, args, "resourceBundle", 7), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        lr,
        "setResourceBundle",
        "(Ljava/util/ResourceBundle;)V",
        |ctx, args| lr_set(ctx, args, "resourceBundle", 7), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        lr,
        "getResourceBundleName",
        "()Ljava/lang/String;",
        |ctx, args| lr_get(ctx, args, "resourceBundleName", 8), cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        lr,
        "setResourceBundleName",
        "(Ljava/lang/String;)V",
        |ctx, args| lr_set(ctx, args, "resourceBundleName", 8), cratonvm_native_api::NativeKind::Intrinsic,
    );
    // `getSequenceNumber` is NOT registered here -- see the note on `getLevel`
    // and `getMessage` above. `setSequenceNumber` below is NOT retired and stays.
    r.register(lr, "setSequenceNumber", "(J)V", |ctx, args| {
        lr_set(ctx, args, "sequenceNumber", 9)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "getThreadID", "()I", |ctx, args| {
        lr_get(ctx, args, "threadID", 10)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "setThreadID", "(I)V", |ctx, args| {
        lr_set(ctx, args, "threadID", 10)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "getThrown", "()Ljava/lang/Throwable;", |ctx, args| {
        lr_get(ctx, args, "thrown", 11)
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(lr, "setThrown", "(Ljava/lang/Throwable;)V", |ctx, args| {
        lr_set(ctx, args, "thrown", 11)
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- Handler (abstract base, 1-field: level=0) ---
    let handler = "java/util/logging/Handler";
    // `setLevel` REFUSES a null level, and the refusal happens BEFORE the
    // store. Both halves are MEASURED (2026-08-17, `scratchpad/g21`, HotSpot
    // 25.0.3+9-LTS vs `--real-jdk`):
    //
    //   Handler.setLevel(null)                          HotSpot NPE msg=null
    //   Handler.getLevel() after the REFUSED setLevel   HotSpot ALL
    //
    // The second row is why the check cannot be "store, then throw": this body
    // used to write the null and return, so `getLevel()` afterwards answered
    // `null` where HotSpot still answers the level that was there. Two
    // assertions of `RJdkIntrinsics3`'s `logrec` family ride on this one body.
    //
    // The NPE is BARE. `Level.parse(null)` in `logmanager.rs` carried an
    // invented `"Name cannot be null"` for months on a row whose CLASS was
    // already right; do not add a message here that HotSpot does not produce.
    // `Handler.setFormatter(null)` and `LogRecord.setLevel(null)` are the same
    // bare shape.
    //
    // NOT A BLANKET JUL RULE, and the surrounding rows prove it. MEASURED on
    // the same oracle run, same probe:
    //
    //   Handler.setFilter(null)      RETURNS   Handler.setEncoding(null) RETURNS
    //   Logger.setLevel(null)        RETURNS   <- the SAME NAME, opposite verdict
    //   Logger.removeHandler(null)   RETURNS   while addHandler(null) throws
    //   LogRecord.set{Message,LoggerName,Parameters,Thrown,ResourceBundle,
    //     ResourceBundleName}(null) RETURN, and so do the retired
    //     set{SourceClassName,SourceMethodName} pair
    //     -- those six ARE the `lr_set` store-then-return bodies a few dozen
    //     lines above, so "add a null check to the JUL setters" would break
    //     six measured rows in THIS function to fix one. Only
    //     `LogRecord.setLevel` and `setInstant` throw, and this file registers
    //     NEITHER: the real bytecode already gets both right.
    //
    // The shape underneath: a JUL method throws when its own first statement
    // dereferences the argument, and returns when the argument is merely
    // stored. `Handler.setLevel` opens `if (newLevel == null) throw`; on
    // `Logger` a null level MEANS "inherit from the parent" and is a legal
    // state. That explains the difference; it does not license deriving any
    // other row from it. G21-1, G15-1 §2, HANDOFF-20260814 §5.
    //
    // THE `setLevel` AND `getLevel` NATIVES ARE GONE from this file as of
    // 2026-09-12, deleted with the three `LogRecord` rows above. The contract
    // above survives its registration because it is about the CONTRACT, which
    // the real bytecode now has to satisfy and which the next person tempted to
    // add a null check to a JUL setter needs to read. It is still under test:
    // `jul_handler_set_level_*` in this file's `t2_tests` now score
    // `reflect_annotations::register_annotation_overrides`, the registration
    // that owns the slot in compatible mode and carries the same measurement.
    //
    // What the old note here said, and why it was the bug rather than the
    // rationale: "In `Compatible` this registration does not own the slot --
    // `reflect_annotations.rs` overwrites it (MEASURED: `owns_slot=false inv=0`
    // here, `owns_slot=true inv=11` there). `--real-jdk` never runs
    // `register_synthetic_overrides`, so THIS body is the only one, and it owns
    // the slot (MEASURED: `kind=intrinsic owns_slot=true overwrote=null
    // inv=12`). The `java/util/logging/` shadow retirement does not reach it
    // either: `retired_shadow.rs` lists the triple, but that retag fires only on
    // an effective category of `Bridge` and this function's ambient category is
    // `Intrinsic`."
    //
    // Every clause of that is true and together they describe an INERT
    // retirement: the row is tabled, the census reports no surviving stub, and
    // the native still dispatches. Dead in compatible mode, sole owner in
    // strict mode -- the exact inverse of what the table asks for.
    // W2: `Handler.close()`/`flush()` are ABSTRACT in the real JDK. An abstract
    // method takes the `check_override` branch, which marks the site native
    // regardless and caches by RECEIVER class, so these two no-ops did not just
    // serve a bare synthetic `Handler` — they intercepted every concrete
    // subclass that did not have its own native. `FileHandler.close()` then
    // never flushed or released its file descriptor and `StreamHandler.flush()`
    // never reached the stream, which is the same "stale JUL stub silently ate
    // the output" shape the `ConsoleHandler`/`SimpleFormatter` removals a few
    // lines below already document.
    //
    // Run the receiver's own bytecode instead. The only receiver that has none
    // is a bare `java/util/logging/Handler` (abstract — javac will not let user
    // code instantiate it; `logmanager.rs` allocates one directly in a test),
    // so that exact class keeps the historical no-op.
    r.register(handler, "close", "()V", |ctx, args| {
        jul_handler_delegate(ctx, args, "close")
    }, cratonvm_native_api::NativeKind::Intrinsic);
    r.register(handler, "flush", "()V", |ctx, args| {
        jul_handler_delegate(ctx, args, "flush")
    }, cratonvm_native_api::NativeKind::Intrinsic);

    // --- ConsoleHandler ---
    // Previously `<init>`/`publish`/`close`/`flush` were all stubbed here:
    // `<init>` was a bare no-op (skipping the real `StreamHandler`
    // constructor's `setOutputStream(System.err)` + property-driven
    // formatter setup entirely, leaving `getFormatter()` null), and
    // `publish` read `LogRecord` raw slot 1 (this module's OLD synthetic
    // `LogRecord` layout, where slot 1 was the message) and fed it to
    // `record_printed_line` -- an internal Rust-side debug buffer nothing
    // in a real Java program (including Spring Boot's `CapturedOutput`
    // test infra) ever reads. The rest of the JUL bridge (logmanager.rs,
    // T19.H3) moved `LogRecord` to a real-field-name layout
    // (`get/set_field_by_name(_, "message")`) long ago, so slot 1 no
    // longer holds the message on records built via
    // `publish_to_jul_handlers_src`'s `new_object_initialized` path --
    // this stub had silently gone stale and made every `ConsoleHandler`
    // constructed by a real `readConfiguration`-driven `logging.properties`
    // (e.g. Spring Boot's `JavaLoggingSystem`) produce zero visible output
    // no matter what handler/level/formatter wiring was otherwise correct.
    // Real `StreamHandler`/`Handler` bytecode verified working directly
    // (`new StreamHandler(System.err, new SimpleFormatter())` correctly
    // formats and writes through the current `System.err`, including under
    // JUnit's `CapturedOutput` wrapping) -- let it run instead of stubbing.

    // --- Formatter (abstract) ---
    //
    // `formatMessage` is NOT an `Intrinsic`, unlike this function's ambient
    // category: JDK 25's body resolves the resource bundle and then runs
    // `java.text.MessageFormat.format` whenever the message contains `{n}` and
    // the record carries parameters, where this body USED TO return
    // `getMessage()` verbatim. Measured then: HotSpot `one=A two=B`, CratonVM
    // `one={0} two={1}`, in BOTH modes. `Intrinsic` is exempt from the
    // `java/util/logging/` shadow retirement, so under `--real-jdk` this
    // survived every honest row being refused — a refusal is not a removal,
    // and re-tagging it `Bridge` did not remove it either: the retirement is
    // driven by the explicit triple table in `native-api/src/retired_shadow.rs`
    // and this triple is not in it, so the row still dispatched in both modes
    // and still answered with the raw pattern.
    // W7-35-jul-supplier-and-payload-residuals.md
    // W7-43-formatmessage-substitution.md
    let fmt = "java/util/logging/Formatter";
    {
        // Two sessions fixed this row concurrently. The version kept is the one
        // that reproduces `java.util.logging.Formatter.formatMessage` in full —
        // ResourceBundle localization with the `MissingResourceException`
        // drop-through, the null-message answer, and the JDK's own fenced
        // `indexOf('{')`+digit scan — rather than the `{0`..`{3` sniff, which
        // misses `{4` and above and skips localization entirely.
        r.register(
            fmt,
            "formatMessage",
            "(Ljava/util/logging/LogRecord;)Ljava/lang/String;",
            |ctx, args| match args.get(1) {
                Some(Value::Object(Some(rec))) => jul_formatter_format_message(ctx, *rec),
                // A null record is an NPE on HotSpot (`record.getMessage()` is
                // the method's first act). This arm used to answer with the
                // EMPTY STRING, and its comment used to say "nothing measured
                // exercises the null, so the historical answer is kept". That
                // is no longer true: MEASURED 2026-08-17 (`scratchpad/g21`),
                // HotSpot 25.0.3+9-LTS throws the helpful NPE below, and
                // CratonVM returned `""` in BOTH modes — this registration owns
                // the slot in both (`owns_slot=true overwrote=null inv=2`).
                //
                // The message is TRANSCRIBED from that run, not derived. It is
                // a helpful-NPE naming the parameter `record`, unlike the BARE
                // NPE of `Handler.setLevel(null)` a few dozen lines above; the
                // two rows are in the same family and do not share a shape.
                // G21-1 N2 / G15-1 §6 N2.
                _ => Err(RuntimeError::NullPointerException {
                    message: Some(JUL_NPE_NULL_FORMAT_RECORD.to_string()),                }
                .into()),            }, cratonvm_native_api::NativeKind::Bridge,
        );
    };

    // --- SimpleFormatter ---
    // Previously `<init>`/`format` were stubbed here the same way
    // `ConsoleHandler.<init>`/`publish` were (see the removal note above
    // `register_spring_boot_logback_apply`'s sibling in lib.rs and the
    // `ConsoleHandler` removal in this file): `<init>` was a bare no-op
    // (skipping the real no-arg constructor's `invokedynamic`-driven
    // default-format-pattern setup entirely), and `format` read `LogRecord`
    // raw slot 1 -- this module's OLD synthetic layout, where slot 1 was
    // the message; the REAL layout (`get/set_field_by_name(_, "message")`,
    // used throughout `logmanager.rs`) has moved the message elsewhere, so
    // slot 1 no longer holds it. The read always missed, falling through to
    // a hardcoded `"INFO: \n"` regardless of the record's actual level or
    // message -- misdiagnosed for a time as a VM interpreter/JIT
    // correctness bug (see the retraction in
    // exception-table-method-state-loss-cluster.md)
    // before this stale stub was found. Real `Formatter`/`SimpleFormatter`
    // bytecode verified working directly for the message/level path (via
    // `Formatter.formatMessage`, already natively bridged further down in
    // this same file); the real no-arg constructor's own
    // `invokedynamic`-driven default pattern remains a separate, narrower,
    // documented residual (`javaloggingsystemtests-simpleformatter-args-drop.md`,
    // corrected likewise) that only affects handlers with no explicit
    // `.formatter=` config -- `apply_jul_config_entries` in
    // `logmanager.rs` already patches around it for that specific case.

    // --- LogManager (singleton) ---
    //
    // NOT `Intrinsic`, unlike the rest of this function, and the distinction is
    // load-bearing rather than cosmetic. These two FABRICATE a manager and a
    // logger in front of real bytecode — a `Bridge` by definition; an intrinsic
    // is the kind that cannot give an answer the bytecode would not.
    //
    // `Intrinsic` is exempt from the `java/util/logging/` shadow retirement, so
    // while the function-wide ambient category applied here these rows SURVIVED
    // `--real-jdk` after the retirement refused every OTHER registrar of the
    // same triples — and a refusal does not remove the registration it would
    // have overwritten. What was left holding `getLogManager()` was an uncached
    // fabricator returning a fresh, unconstructed manager on every call
    // (identityHashCode 7, 8, 9 on three successive calls), whose null
    // `systemContext` is the first NPE any JUL user hits.
    //
    // The real `LogManager.<init>` is itself retired, so the static
    // `LogManager.manager` already comes out of the real constructor — this
    // builds no state, it stops shadowing state that is already there.
    // W7-25-jul-getlogger-regression.md
    let lm = "java/util/logging/LogManager";
    {
        r.register(
            lm,
            "getLogManager",
            "()Ljava/util/logging/LogManager;",
            |ctx, _args| {
                let obj = try_alloc_concurrent_synthetic(ctx, "java/util/logging/LogManager", 0)?;
                Ok(Some(Value::Object(Some(obj))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            lm,
            "getLogger",
            "(Ljava/lang/String;)Ljava/util/logging/Logger;",
            |ctx, _args| {
                // Return a new Logger stub
                let logger = try_alloc_concurrent_synthetic(ctx, "java/util/logging/Logger", 2)?;
                Ok(Some(Value::Object(Some(logger))))            }, cratonvm_native_api::NativeKind::Bridge,
        );
    };
    }

/// HotSpot's helpful-NPE text for `Formatter.formatMessage(null)`.
///
/// TRANSCRIBED from a HotSpot 25.0.3+9-LTS run (`scratchpad/g21`, 2026-08-17),
/// not derived: `formatMessage`'s first act is `record.getMessage()`, and the
/// helpful-NPE machinery names the receiver expression and the parameter. The
/// class alone is not enough to assert — `Level.parse(null)` shipped the right
/// class with an invented `"Name cannot be null"` for months (G15-1 §4).
pub const JUL_NPE_NULL_FORMAT_RECORD: &str =
    "Cannot invoke \"java.util.logging.LogRecord.getMessage()\" because \"record\" is null";

/// Does the message look like a `java.text` format string?
///
/// This is JDK 25 `Formatter.formatMessage`'s own guard, and its own comment
/// says why it is written this way rather than as `Pattern.compile("\\{\\d")`:
/// the regex costs 14% more, so the JDK walks `indexOf('{')` and looks at the
/// next `char`. Reproduced exactly, including the `index >= fence` break that
/// makes a `{` in the LAST position not a pattern.
///
/// Byte-wise is equivalent to the JDK's `char`-wise scan here and not an
/// approximation of it: `{` and `0`-`9` are ASCII, and UTF-8 never encodes an
/// ASCII byte inside a multi-byte sequence, so a `{` byte is a `{` char and the
/// byte after it is the next char's first byte.
pub fn jul_message_is_java_text_format(message: &str) -> bool {
    let bytes = message.as_bytes();
    let mut index = 0usize;
    // `fence = length - 1`; the JDK breaks when a `{` is found AT the fence,
    // which is the same as never looking past the last byte.
    while index + 1 < bytes.len() {
        if bytes[index] == b'{' && bytes[index + 1].is_ascii_digit() {
            return true;
        }
        index += 1;
    }
    false
}

/// A `String` result, or the null the JDK returns when the record's message is
/// null. `formatMessage` is declared to return `String`, and HotSpot really
/// does hand back `null` for `new LogRecord(level, null)` — the empty string
/// this row used to answer with is not the same value and prints differently
/// through `SimpleFormatter.format`.
pub fn jul_message_result(ctx: &mut dyn NativeContext, message: Option<String>) -> Value {
    match message {
        Some(text) => {
            let s = ctx.create_string(&text);
            Value::Object(Some(s))
        }
        None => Value::Object(None),
    }
}

/// `java.util.logging.Formatter.formatMessage(LogRecord)`.
///
/// The registration this serves used to be `record.getMessage()` and nothing
/// else, which is why the strict corpus' `RJdkLogging.recordPayloads` read back
/// `one={0} two={1}` where HotSpot reads `one=A two=B`. The record side was
/// never at fault — the same fixture asserts, and passes, that the record keeps
/// the RAW pattern and both parameters. HotSpot substitutes in the FORMATTER.
///
/// The five steps below are JDK 25's `Formatter.formatMessage` in order, read
/// off `java.logging/java/util/logging/Formatter.java` in the image's
/// `src.zip`, not from memory:
///
/// 1. `String format = record.getMessage();`
/// 2. localize it through `record.getResourceBundle()` when there is one,
///    keeping the original if the lookup misses.
/// 3. `record.getParameters()`; a null or empty array returns the message with
///    NO formatting at all — this is why a bare `{0}` in an unparameterized
///    message survives to the output.
/// 4. the cheap `{<digit>` probe ([`jul_message_is_java_text_format`]); a
///    message that does not look like a pattern is returned unchanged.
/// 5. `java.text.MessageFormat.format(format, parameters)` — **and if that
///    throws, the original message is returned**. The JDK swallows it on
///    purpose ("Formatting failed: use localized format string"), which is what
///    makes a bad pattern such as `set={x} v={0}` come back verbatim instead of
///    propagating an `IllegalArgumentException` into a logging call.
///
/// Step 5 is the trap this fix had to rule out before touching step 4: a
/// missing or stubbed `MessageFormat` would throw, the swallow would turn that
/// into "return the raw pattern", and the symptom would be identical with the
/// guard entirely innocent. It is not that here — measured on the pre-fix dev
/// binary, `MessageFormat.format("one={0} two={1}", {"A","B"})` answers
/// `one=A two=B` in BOTH `--fast-boot` and `--real-jdk`, and matches HotSpot on
/// the quoting, out-of-range-index and bad-argument-name cases too (it throws
/// `IllegalArgumentException` on `{x}` exactly as HotSpot does). So the
/// dependency was healthy and the shadow was simply not calling it.
/// W7-43-formatmessage-substitution.md
pub fn jul_formatter_format_message(
    ctx: &mut dyn NativeContext,
    record: ObjectRef,
) -> MethodCallResult {
    // Every step below re-enters Java and can move the heap, so the record is
    // rooted once and re-read through its pin after each re-entry (the native
    // stale-local family). One base pin, one unpin on every exit path.
    let base_pin = ctx.pin_native_root(record);
    let out = jul_formatter_format_message_body(ctx, base_pin, record);
    ctx.unpin_native_roots(base_pin);
    out
}

pub fn jul_formatter_format_message_body(
    ctx: &mut dyn NativeContext,
    rec_pin: usize,
    record: ObjectRef,
) -> MethodCallResult {
    // 1. `String format = record.getMessage();`
    let rec = ctx.read_native_pin(rec_pin, record);
    let message_obj = ctx.invoke_virtual(rec, "getMessage", "()Ljava/lang/String;", &[])?;
    let mut message = match message_obj {
        Some(Value::Object(Some(s))) => ctx.read_string(s),
        _ => None,
    };

    // 2. Localize through the record's own bundle. `getString` raising
    //    `MissingResourceException` is the DOCUMENTED miss path ("Drop
    //    through. Use record message as format"), so a failed lookup keeps the
    //    raw message rather than propagating.
    let rec = ctx.read_native_pin(rec_pin, record);
    let catalog_val = ctx.invoke_virtual(
        rec,
        "getResourceBundle",
        "()Ljava/util/ResourceBundle;",
        &[],
    )?;
    let catalog = match catalog_val {
        Some(Value::Object(o)) => o,
        _ => None,
    };
    if let (Some(catalog), Some(key_text)) = (catalog, message.clone()) {
        let catalog_pin = ctx.pin_native_root(catalog);
        let key = ctx.create_string(&key_text);
        let key_pin = ctx.pin_native_root(key);
        let catalog = ctx.read_native_pin(catalog_pin, catalog);
        let key = ctx.read_native_pin(key_pin, key);
        let localized_call = ctx.invoke_virtual(
            catalog,
            "getString",
            "(Ljava/lang/String;)Ljava/lang/String;",
            &[Value::Object(Some(key))],
        );
        if let Ok(Some(Value::Object(Some(localized)))) = localized_call {
            if let Some(text) = ctx.read_string(localized) {
                message = Some(text);
            }
        }
    }

    // 3. `Object[] parameters = record.getParameters();` — null or empty means
    //    NO formatting is performed at all.
    //    `getParameters()` is INSIDE the JDK's `try`, unlike the two calls
    //    above it, so a throw here is swallowed into "return the message" too.
    let rec = ctx.read_native_pin(rec_pin, record);
    let params_call = ctx.invoke_virtual(rec, "getParameters", "()[Ljava/lang/Object;", &[]);
    let parameters = match params_call {
        Ok(Some(Value::Object(o))) => o,
        Ok(_) => None,
        Err(_) => return Ok(Some(jul_message_result(ctx, message))),
    };
    let Some(parameters) = parameters else {
        return Ok(Some(jul_message_result(ctx, message)));
    };
    let params_pin = ctx.pin_native_root(parameters);
    let params_probe = ctx.read_native_pin(params_pin, parameters);
    if ctx.array_length(params_probe) == 0 {
        return Ok(Some(jul_message_result(ctx, message)));
    }

    // 4. The `{<digit>` probe. A message that is not a pattern is returned
    //    unchanged even though parameters are present.
    let Some(pattern) = message else {
        // A null message WITH parameters: the JDK reaches `format.length()`
        // inside its own `try`, the NPE lands in the catch, and the catch
        // returns `format` — which is null. Not the empty string.
        return Ok(Some(Value::Object(None)));
    };
    if !jul_message_is_java_text_format(&pattern) {
        let s = ctx.create_string(&pattern);
        return Ok(Some(Value::Object(Some(s))));
    }

    // 5. `java.text.MessageFormat.format(format, parameters)`, with the JDK's
    //    deliberate swallow: any failure returns the (localized) message.
    let pattern_obj = ctx.create_string(&pattern);
    let pattern_pin = ctx.pin_native_root(pattern_obj);
    let parameters = ctx.read_native_pin(params_pin, parameters);
    let pattern_obj = ctx.read_native_pin(pattern_pin, pattern_obj);
    let rendered_call = ctx.invoke(
        "java/text/MessageFormat",
        "format",
        "(Ljava/lang/String;[Ljava/lang/Object;)Ljava/lang/String;",
        &[
            Value::Object(Some(pattern_obj)),
            Value::Object(Some(parameters)),
        ],
    );
    match rendered_call {
        Ok(Some(Value::Object(Some(rendered)))) => Ok(Some(Value::Object(Some(rendered)))),
        _ => {
            let s = ctx.create_string(&pattern);
            Ok(Some(Value::Object(Some(s))))
        }
    }
}

// ---------------------------------------------------------------------------
// java.net extras: URI, HttpURLConnection stubs
// ---------------------------------------------------------------------------
/// `URI.<init>(String)`: store `text` (or `null`) into slot `index` of the
/// synthetic URI `this`; answers `this`'s current address (gc-common w20-b:
/// the String is allocated with `this` pinned).
pub fn uri_store_text(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    index: usize,
    text: Option<&str>,
) -> ObjectRef {
    let Some(t) = text else {
        ctx.set_field(this, index, Value::Object(None));
        return this;
    };
    alloc_string_into_field(ctx, this, index, t)
}

/// [`uri_store_text`] for the real `URI` layout's named fields.
pub fn uri_store_named_text(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    text: &str,
) -> ObjectRef {
    let pin = ctx.pin_native_root(this);
    let s = ctx.create_string(text);
    let this = ctx.read_native_pin(pin, this);
    ctx.unpin_native_roots(pin);
    ctx.set_field_by_name(this, name, Value::Object(Some(s)));
    this
}

pub fn register_phase54_net_extras(r: &mut NativeMethodRegistry) {
            // --- java.net.URI (7-field: scheme=0, host=1, port=2, path=3, query=4, fragment=5, raw=6) ---
    let uri = "java/net/URI";
    r.register(uri, "<init>", "(Ljava/lang/String;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw_ref = obj_arg(args, 1)?;
        // The ARGUMENT is the authoritative text, and it must be stored rather
        // than rebuilt. `read_string` decodes to a Rust `String`, which cannot
        // hold an unpaired surrogate; every `create_string(&raw)` below is
        // therefore a lossy round trip. MEASURED on both VMs at `b9f302019`:
        //
        //   new URI("http://h/a<U+D800>b").toString()
        //     HotSpot   len=12  charAt(10)=d800
        //     CratonVM  len=12  charAt(10)=fffd
        //
        // The length was right, which is why this survived so long — only the
        // one code unit was wrong. `raw` stays for the PARSING below, which
        // splits on ASCII delimiters and is unaffected by the substitution;
        // what must not be rebuilt is the text handed back verbatim.
        //
        // Pinned because this closure allocates a dozen strings between here
        // and the stores, and a moving young collection would leave `raw_ref`
        // pointing into from-space.
        //
        // gc-common w20-b: `this` and the component strings were held raw
        // across those same allocations (the hazard the note here left
        // open). The components are now parsed as Rust text first, and each
        // String is stored the moment it is made, into `this` re-read from
        // its pin (`this_pin` is below `raw_pin`, so one unpin drops both).
        let this_pin = ctx.pin_native_root(this);
        let raw_pin = ctx.pin_native_root(raw_ref);
        let raw = ctx.read_string(raw_ref).unwrap_or_default();
        // JDK URI authority is introduced only by `//` after the optional
        // scheme. A plain relative URI such as `docProps/core.xml` is all path;
        // treating `docProps` as an authority drops the first path segment and
        // corrupts OOXML relationship entries when POI constructs package part
        // names through `new URI(String)`.
        let (scheme_text, rest) = match raw.find(':') {
            Some(pos) => {
                let candidate = &raw[..pos];
                let valid_scheme = !candidate.is_empty()
                    && candidate
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_alphabetic())
                        .unwrap_or(false)
                    && candidate
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
                if valid_scheme {
                    (Some(candidate), &raw[pos + 1..])
                } else {
                    (None, raw.as_str())
                }
            }
            None => (None, raw.as_str()),
        };
        let (authority, path_and_rest) = if let Some(after_slashes) = rest.strip_prefix("//") {
            match after_slashes.find(['/', '?', '#']) {
                Some(pos) => (Some(&after_slashes[..pos]), &after_slashes[pos..]),
                None => (Some(after_slashes), ""),
            }
        } else {
            (None, rest)
        };
        let (host_text, port): (Option<&str>, i32) = if let Some(authority) = authority {
            if let Some(colon) = authority.rfind(':') {
                if let Ok(p) = authority[colon + 1..].parse::<i32>() {
                    (Some(&authority[..colon]), p)
                } else {
                    (Some(authority), -1)
                }
            } else if authority.is_empty() {
                (None, -1)
            } else {
                (Some(authority), -1)
            }
        } else {
            (None, -1)
        };
        // Split path?query#fragment
        let (path_str, query, fragment) = {
            let path_and_rest = if scheme_text.is_some()
                && authority.is_none()
                && !path_and_rest.starts_with('/')
            {
                ""
            } else {
                path_and_rest
            };
            let (path_q, frag) = if let Some(hash) = path_and_rest.find('#') {
                (&path_and_rest[..hash], Some(&path_and_rest[hash + 1..]))
            } else {
                (path_and_rest, None)
            };
            let (path_s, q) = if let Some(qmark) = path_q.find('?') {
                (&path_q[..qmark], Some(&path_q[qmark + 1..]))
            } else {
                (path_q, None)
            };
            (path_s, q, frag)
        };
        let path_text = if path_str.is_empty() {
            None
        } else {
            Some(path_str)        };
        let this = ctx.read_native_pin(this_pin, this);
        let this = uri_store_text(ctx, this, 0, scheme_text);
        let this = uri_store_text(ctx, this, 1, host_text);
        ctx.set_field(this, 2, Value::Int(port));
        let this = uri_store_text(ctx, this, 3, path_text);
        let this = uri_store_text(ctx, this, 4, query);
        let this = uri_store_text(ctx, this, 5, fragment);
        // Slot 6 and `string` are the same text and may be the same object:
        // `java.lang.String` is immutable, and HotSpot's `URI` likewise keeps
        // one reference to the string it was constructed from.
        let raw_str = ctx.read_native_pin(raw_pin, raw_ref);
        ctx.set_field(this, 6, Value::Object(Some(raw_str)));
        ctx.set_field_by_name(this, "string", Value::Object(Some(raw_str)));
        if let Some(scheme) = scheme_text {
            let this = uri_store_named_text(ctx, this, "scheme", scheme);
            let raw_ssp = rest.split('#').next().unwrap_or(rest);
            let this = uri_store_named_text(ctx, this, "schemeSpecificPart", raw_ssp);
            uri_store_named_text(ctx, this, "decodedSchemeSpecificPart", raw_ssp);
        }
        if !path_str.is_empty() {
            let this = ctx.read_native_pin(this_pin, this);
            let this = uri_store_named_text(ctx, this, "path", path_str);
            uri_store_named_text(ctx, this, "decodedPath", path_str);
        }
        ctx.unpin_native_roots(this_pin);
        Ok(Some(Value::Object(None)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        uri,
        "create",
        "(Ljava/lang/String;)Ljava/net/URI;",
        |ctx, args| {
            let str_ref = obj_arg(args, 0)?;
            let raw = ctx.read_string(str_ref).unwrap_or_default();
            // Parse scheme
            let (scheme, rest) = if let Some(pos) = raw.find("://") {
                (Some(&raw[..pos]), &raw[pos + 3..])
            } else {
                (None, raw.as_str())
            };
            let (authority, path_and_rest) = if let Some(pos) = rest.find('/') {
                (&rest[..pos], &rest[pos..])
            } else {
                (rest, "")
            };
            let (host, port) = if let Some(colon) = authority.rfind(':') {
                if let Ok(p) = authority[colon + 1..].parse::<i32>() {
                    (Some(&authority[..colon]), p)
                } else {
                    (Some(authority), -1)
                }
            } else if authority.is_empty() {
                (None, -1)
            } else {
                (Some(authority), -1)
            };
            let path_text = (!path_and_rest.is_empty()).then_some(path_and_rest);
            let obj = try_alloc_concurrent_synthetic(ctx, "java/net/URI", 7)?;
            // gc-common w19-d handoff: the URI is rooted across every String
            // below and across `uri_publish_named` (which allocates a dozen
            // and pins the URI only inside itself); each String is stored as
            // soon as it is made, through the URI's current address, and the
            // caller gets the address after the last allocation. This used to
            // hold the URI and five Strings raw across each other's
            // allocation and return the URI's pre-publish address.
            let synthetic = crate::net_phase_e::uri_has_synthetic_layout(ctx, obj);
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let obj_h = scope.root(obj);
            // JDK-ONLY-LAYOUT: raw slots only on OUR layout. On a real
            // `java.net.URI` slots 1..6 are `fragment, authority, userInfo,
            // host, port, path`, so this block wrote the host into the
            // fragment, the port into the authority and the raw text into the
            // path — six writes, five of them the wrong field.
            if synthetic {
                let string_slots = [
                    (0, scheme),
                    (1, host),
                    (3, path_text),
                    (6, Some(raw.as_str())),
                ];
                for (slot, text) in string_slots {
                    let v = match text {
                        Some(t) => Value::Object(Some(scope.create_string(t))),
                        None => Value::Object(None),
                    };
                    let obj_now = scope.get(&obj_h);
                    scope.set_field(obj_now, slot, v);
                }
                let obj_now = scope.get(&obj_h);
                scope.set_field(obj_now, 2, Value::Int(port));
                scope.set_field(obj_now, 4, Value::Object(None));
                scope.set_field(obj_now, 5, Value::Object(None));
            }
            let obj_now = scope.get(&obj_h);
            crate::net_phase_e::uri_publish_named(&mut *scope, obj_now, &raw, None);
            Ok(Some(Value::Object(Some(scope.get(&obj_h)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(uri, "getScheme", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "getHost", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "getPath", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let raw = crate::net_phase_e::uri_raw_string(ctx, this);
        let path = match crate::net_phase_e::uri_select_raw_path(&raw) {
            Some(path) => path,
            None => return Ok(Some(Value::Object(None))),
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&path)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "getQuery", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 4)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "getFragment", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 5)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // residual-3 fix: field 6 is only populated by the `<init>`/`create`
        // constructors below; URI objects built via `url_parse`+`uri_store_named`
        // (e.g. `File.toURI()`, `phases_late.rs`'s `Path.toUri()` helpers) only
        // set the "string" field BY NAME plus field 5 (`URL_FIELD_FULL`), so a
        // bare `get_field(this, 6)` silently reads an unset slot for those.
        // `uri_raw_string` (net_phase_e.rs) already tries the by-name "string"
        // field, then indices 6 and 5, then a guarded index-0 fallback — a
        // strict superset that still finds field 6 for the `<init>`/`create`
        // convention, so this is a pure widening, not a behavior change for
        // the previously-working case.
        let s = crate::net_phase_e::uri_raw_string(ctx, this);
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "toASCIIString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = crate::net_phase_e::uri_raw_string(ctx, this);
        Ok(Some(Value::Object(Some(ctx.create_string(&s)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `URI.toURL()` is registered by `net_phase_e` with the real URL-aware
    // implementation. Do not override it here: this late phase used to
    // replace it with a one-slot synthetic URL, so URL consumers eventually
    // dispatched `toExternalForm()` on a plain Object and intermittently
    // dropped Tomcat HTTP/2 responses.
    // equals/hashCode: this registration (phase54, registered LAST — see the
    // `register_phase54_natives` call site in lib.rs) wins over the identical
    // pair in `net_phase_e.rs`, so it is the one actually in effect for
    // real-JDK mode. Delegates to the shared `net_phase_e::uri_equals`/
    // `uri_hash_code` helpers (component-wise, JDK-matching semantics —
    // scheme/host compared case-insensitively, everything else exactly) —
    // see their doc comments for why raw-string equality was wrong (it broke
    // `UriComponentsTests::toUriWithIpv6HostAlreadyEncoded[WHAT_WG]`, whose
    // WHATWG-canonicalized lowercase IPv6 host is still `URI.equals()` to the
    // mixed-case original per real JDK's case-insensitive host comparison).
    r.register(uri, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Object(Some(other)) = args[1] {
            let eq = crate::net_phase_e::uri_equals(ctx, this, other);
            Ok(Some(Value::Int(if eq { 1 } else { 0 })))
        } else {
            Ok(Some(Value::Int(0)))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(uri, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(crate::net_phase_e::uri_hash_code(
            ctx, this,
        ))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- HttpURLConnection (10-field) ---
    // url=0, method=1, responseCode=2, fd_id=3, reqHeaders=4 (HashMap-like array),
    // respHeaders=5 (array of "Key: Value" strings), respBody=6 (byte[]),
    // doInput=7, doOutput=8, connected=9
    let huc = "java/net/HttpURLConnection";

    // residual-4 fix: `getResponseCode`/`getRequestMethod`/`setRequestMethod`/
    // `setRequestProperty`/`addRequestProperty` are intentionally NOT
    // re-registered here. This function (`register_phase54_net_extras`) runs
    // AFTER `http_url_connection::register_http_url_connection_real` (see
    // `lib.rs`'s `register_essential_natives` vs `register_synthetic_overrides`
    // call order), so a duplicate registration here would win and silently
    // shadow it. The `http_url_connection.rs` versions are a strict superset:
    // they branch on `is_real_carrier` and store custom headers in the
    // identity-keyed `real_reqs` side table for a real-JDK URL carrier (this
    // MockWebServer/HttpURLConnection-based case), falling back to the same
    // synthetic field-4 array this file used for the non-real-carrier case.
    // The old duplicate here always wrote to field 4 regardless of carrier
    // kind, so a header set via `customizeConnection(HttpURLConnection)` on a
    // real carrier never reached `huc_real_perform`'s request — the request
    // dispatched with the caller-set headers missing entirely (Spring's
    // `UrlResource` `customizeConnection`/`exists()` HEAD-then-GET fallback
    // tests: `canCustomizeHttpUrlConnectionForExists[Fallback]`).

    r.register(huc, "setDoInput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        ctx.set_field(this, 7, Value::Int(v));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "setDoOutput", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        // Real-JDK carrier (slot 0 holds the real `URLConnection.url`): set the
        // REAL `doOutput` field by name so inherited `getDoOutput()` reports it.
        // Writing the synthetic slot 8 on a real object aliases `readTimeout`
        // and leaves `doOutput` false → Spring skips the request body → server
        // "Read timed out". See the twin fix in net_phase_e::register_re4_url_http.
        if matches!(ctx.get_field(this, 0), Value::Object(Some(_))) {
            ctx.set_field_by_name(this, "doOutput", Value::Int(v));        } else {
            ctx.set_field(this, 8, Value::Int(v));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "connect", "()V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
            p54_huc_do_request(ctx, &mut this)?;
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        huc,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            // Auto-connect
            if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
                p54_huc_do_request(ctx, &mut this)?;
            }
            // Return ByteArrayInputStream wrapping response body
            let body_arr: ObjectRef = match ctx.get_field(this, 6) {
                Value::Object(Some(a)) => a,
                _ => ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0),
            };
            let len = ctx.array_length(body_arr);
            // gc-common w20-b: the body was held raw across the stream's
            // allocation (a class resolution, which can run `<clinit>`).
            let body_pin = ctx.pin_native_root(body_arr);
            let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4);
            let body_arr = ctx.read_native_pin(body_pin, body_arr);
            ctx.unpin_native_roots(body_pin);
            let stream: ObjectRef = stream?;
            ctx.set_field(stream, 0, Value::Object(Some(body_arr))); // buf
            ctx.set_field(stream, 1, Value::Int(0)); // pos
            ctx.set_field(stream, 2, Value::Int(0)); // mark
            ctx.set_field(stream, 3, Value::Int(len as i32)); // count
            Ok(Some(Value::Object(Some(stream))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        huc,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 8, Value::Int(1)); // doOutput = true
            let baos = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayOutputStream", 2)?;
            // gc-common w20-b: the stream was held raw across its buffer.
            let baos: ObjectRef =
                alloc_array_into_field(ctx, baos, 0, cratonvm_types::ArrayElementType::Byte, 4096);
            ctx.set_field(baos, 1, Value::Int(0));
            Ok(Some(Value::Object(Some(baos))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        huc,
        "getHeaderField",
        "(Ljava/lang/String;)Ljava/lang/String;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
                p54_huc_do_request(ctx, &mut this)?;
            }
            let key_ref = obj_arg(args, 1)?;
            let key = ctx.read_string(key_ref).unwrap_or_default().to_lowercase();
            if let Value::Object(Some(hdr_arr)) = ctx.get_field(this, 5) {
                let len = ctx.array_length(hdr_arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(hdr_arr, i) {
                        let line = ctx.read_string(s).unwrap_or_default();
                        if let Some(colon) = line.find(':') {
                            if line[..colon].trim().to_lowercase() == key {
                                let val = line[colon + 1..].trim();
                                let vs = ctx.create_string(val);
                                return Ok(Some(Value::Object(Some(vs))));                            }                        }                    }                }            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(huc, "getContentLength", "()I", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
            p54_huc_do_request(ctx, &mut this)?;
        }
        match ctx.get_field(this, 6) {
            Value::Object(Some(a)) => Ok(Some(Value::Int(ctx.array_length(a) as i32))),
            _ => Ok(Some(Value::Int(-1))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "getContentLengthLong", "()J", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
            p54_huc_do_request(ctx, &mut this)?;
        }
        match ctx.get_field(this, 6) {
            Value::Object(Some(a)) => Ok(Some(Value::Long(ctx.array_length(a) as i64))),
            _ => Ok(Some(Value::Long(-1))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        huc,
        "getContentType",
        "()Ljava/lang/String;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
                p54_huc_do_request(ctx, &mut this)?;
            }
            // Search response headers for Content-Type
            if let Value::Object(Some(hdr_arr)) = ctx.get_field(this, 5) {
                let len = ctx.array_length(hdr_arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(hdr_arr, i) {
                        let line = ctx.read_string(s).unwrap_or_default();
                        if let Some(colon) = line.find(':') {
                            if line[..colon].trim().eq_ignore_ascii_case("content-type") {
                                let val = line[colon + 1..].trim();
                                let vs = ctx.create_string(val);
                                return Ok(Some(Value::Object(Some(vs))));                            }                        }                    }                }            }
            Ok(Some(Value::Object(None)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(huc, "disconnect", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let fd_id = ctx.get_field(this, 3).as_int().unwrap_or(-1);
        if fd_id >= 0 {
            // Ignore close errors: the FD may already be gone if a
            // concurrent path closed it first.
            let _ = ctx.fd_table().close(fd_id as u32);
            ctx.set_field(this, 3, Value::Int(-1));
        }
        ctx.set_field(this, 9, Value::Int(0));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "getURL", "()Ljava/net/URL;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // HttpURLConnection instance setters. These were `native_noop_with_this`,
    // and because `register_phase54_net_extras` runs AFTER
    // `http_url_connection::register_http_url_connection_real` (see the
    // residual-4 note above) the no-ops WON: a caller's
    // `setConnectTimeout(1234)` / `setReadTimeout(5678)` was discarded and the
    // matching getter reported the unset default. Record the values on the
    // carrier's free slots and serve the getters from them; `p54_huc_do_request`
    // applies the read timeout to the socket.
    r.register(huc, "setConnectTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if v < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "timeout can not be negative".to_string(),
            }
            .into());
        }
        p54_huc_set_slot(ctx, this, P54_HUC_CONNECT_TIMEOUT, Value::Int(v));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "getConnectTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = p54_huc_get_slot(ctx, this, P54_HUC_CONNECT_TIMEOUT)
            .as_int()
            .unwrap_or(0);
        Ok(Some(Value::Int(v)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "setReadTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if v < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "timeout can not be negative".to_string(),
            }
            .into());
        }
        p54_huc_set_slot(ctx, this, P54_HUC_READ_TIMEOUT, Value::Int(v));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "getReadTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = p54_huc_get_slot(ctx, this, P54_HUC_READ_TIMEOUT)
            .as_int()
            .unwrap_or(0);
        Ok(Some(Value::Int(v)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        huc,
        "getResponseMessage",
        "()Ljava/lang/String;",
        |ctx, args| {
            let mut this = obj_arg(args, 0)?;
            if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 {
                p54_huc_do_request(ctx, &mut this)?;
            }
            let code = ctx.get_field(this, 2).as_int().unwrap_or(0);
            let msg = match code {
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
                500 => "Internal Server Error",
                502 => "Bad Gateway",
                503 => "Service Unavailable",
                _ => "Unknown",
            };
            let s = ctx.create_string(msg);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // getErrorStream() — the 4xx/5xx response body. This used to be a constant
    // `null`, which (like the timeout setters above) SHADOWED
    // `http_url_connection.rs`'s real `huc_get_error_stream`, because
    // `register_phase54_net_extras` runs after
    // `register_http_url_connection_real`. Callers that read the error body to
    // build a diagnostic (`RestTemplate` error handlers, `URLConnection`-based
    // clients) got nothing at all on every failed request.
    //
    // The body is already in slot 6 of this file's 16-slot carrier (the one
    // `net_phase_e::register_re4_url_http` allocates, whose HUC_* constants are
    // identical to the layout documented at the top of this block), so serve it
    // from there, mirroring the `getInputStream` registration ~200 lines above.
    //
    // Per the javadoc this must NOT initiate a connection: an unconnected
    // carrier, a success status, or no buffered body all answer null.
    r.register(
        huc,
        "getErrorStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Layout guard: `java/net/HttpURLConnection` carries two synthetic
            // layouts in this tree. In this file's 16-slot one, slot 2 is the
            // response code (an Int); in `http_url_connection.rs`'s 12-slot one
            // it is the request-method String. Anything that is not this layout
            // keeps the previous `null` answer rather than misreading slots.
            if ctx.object_num_fields(this) < 10 {
                return Ok(Some(Value::Object(None)));
            }
            let code = match ctx.get_field(this, 2) {
                Value::Int(c) => c,
                _ => return Ok(Some(Value::Object(None))),
            };
            if ctx.get_field(this, 9).as_int().unwrap_or(0) == 0 || code < 400 {
                return Ok(Some(Value::Object(None)));
            }
            let body_arr = match ctx.get_field(this, 6) {
                Value::Object(Some(a)) => a,
                _ => return Ok(Some(Value::Object(None))),
            };
            let len = ctx.array_length(body_arr) as i32;
            // Pin the body across the stream allocation — a moving young GC
            // there would relocate it (native stale-local family).
            let body_pin = ctx.pin_native_root(body_arr);
            let stream = try_alloc_concurrent_synthetic(ctx, "java/io/ByteArrayInputStream", 4)?;
            let body_arr = ctx.read_native_pin(body_pin, body_arr);
            ctx.unpin_native_roots(body_pin);
            ctx.set_field(stream, 0, Value::Object(Some(body_arr))); // buf
            ctx.set_field(stream, 1, Value::Int(0)); // pos
            ctx.set_field(stream, 2, Value::Int(0)); // mark
            ctx.set_field(stream, 3, Value::Int(len)); // count
            Ok(Some(Value::Object(Some(stream))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // More HUC instance setters — same shadowing story as the timeouts above.
    r.register(huc, "setInstanceFollowRedirects", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        p54_huc_set_slot(
            ctx,
            this,
            P54_HUC_NO_REDIRECTS,
            Value::Int(i32::from(v == 0)),
        );
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "getInstanceFollowRedirects", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let disabled = p54_huc_get_slot(ctx, this, P54_HUC_NO_REDIRECTS)
            .as_int()
            .unwrap_or(0);
        Ok(Some(Value::Int(i32::from(disabled == 0))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "setUseCaches", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        p54_huc_set_slot(ctx, this, P54_HUC_NO_CACHES, Value::Int(i32::from(v == 0)));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "getUseCaches", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let disabled = p54_huc_get_slot(ctx, this, P54_HUC_NO_CACHES)
            .as_int()
            .unwrap_or(0);
        Ok(Some(Value::Int(i32::from(disabled == 0))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        huc,
        "setFixedLengthStreamingMode",
        "(I)V",
        p54_huc_set_fixed_length_streaming_mode, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        huc,
        "setFixedLengthStreamingMode",
        "(J)V",
        p54_huc_set_fixed_length_streaming_mode, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        huc,
        "setChunkedStreamingMode",
        "(I)V",
        p54_huc_set_chunked_streaming_mode, cratonvm_native_api::NativeKind::Bridge,
    );

    // HTTP response code constants — KEEP. `static final int` FIELD reads (the
    // "I" field descriptor), whose values are fixed by RFC 9110 and declared
    // literally in `java.net.HttpURLConnection`.
    r.register(huc, "HTTP_OK", "I", |_ctx, _args| Ok(Some(Value::Int(200))), cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_CREATED", "I", |_ctx, _args| {
        Ok(Some(Value::Int(201)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_NO_CONTENT", "I", |_ctx, _args| {
        Ok(Some(Value::Int(204)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_NOT_FOUND", "I", |_ctx, _args| {
        Ok(Some(Value::Int(404)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_INTERNAL_ERROR", "I", |_ctx, _args| {
        Ok(Some(Value::Int(500)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_BAD_REQUEST", "I", |_ctx, _args| {
        Ok(Some(Value::Int(400)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_UNAUTHORIZED", "I", |_ctx, _args| {
        Ok(Some(Value::Int(401)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_FORBIDDEN", "I", |_ctx, _args| {
        Ok(Some(Value::Int(403)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_MOVED_PERM", "I", |_ctx, _args| {
        Ok(Some(Value::Int(301)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_MOVED_TEMP", "I", |_ctx, _args| {
        Ok(Some(Value::Int(302)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_NOT_MODIFIED", "I", |_ctx, _args| {
        Ok(Some(Value::Int(304)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_BAD_GATEWAY", "I", |_ctx, _args| {
        Ok(Some(Value::Int(502)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(huc, "HTTP_UNAVAILABLE", "I", |_ctx, _args| {
        Ok(Some(Value::Int(503)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // --- InetAddress additions ---
    //
    // `java.net.InetAddress` is a real bootstrap class: its instance slots
    // 0/1 are the `holder` reference fields, NOT `hostName` / `address`
    // Strings. CratonVM-synthesised InetAddress objects keep their host/IP
    // in the `net_phase_e` ObjectRef-keyed side table; these natives must
    // consult it rather than reading the (intentionally unpopulated)
    // instance slots — reading a String out of the `holder` slot is exactly
    // what caused bogus `NoSuchMethodError java/lang/String.getHostName()`.
    let ia = "java/net/InetAddress";
    r.register(ia, "getHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Layout-aware: side table → real-JDK `holder` → legacy slot 0.
        //
        // A mirror that carries NO hostName stores the empty string (see
        // `net_phase_e::NO_HOST_NAME`); this accessor must then answer the
        // numeric text, never "". HotSpot arrives at the same place by trying a
        // reverse lookup and falling back to `getHostAddress()`. This
        // duplicates `net_phase_e::inet_addr_host_name_value` because both
        // registrations exist and either may win the last-writer-wins registry
        // slot — they must not disagree.
        let host = match crate::net_phase_e::inet_addr_resolve(ctx, this) {
            Some((h, _)) => h,
            None => match ctx.get_field(this, 0) {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            },
        };
        // The no-name fallback is `getHostAddress()` — scope suffix included.
        let answer = if host.is_empty() {
            crate::net_phase_e::inet_addr_scoped_text(ctx, this, "")
        } else {
            host
        };
        Ok(Some(Value::Object(Some(ctx.create_string(&answer)))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ia, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::net_phase_e::inet_addr_resolve(ctx, this).is_some() {
            // Shared with the `net_phase_e` registrations so the IPv6 scope
            // suffix cannot depend on which duplicate won the registry slot.
            let v = crate::net_phase_e::inet_addr_host_address_value(ctx, this);
            if let Value::Object(Some(s)) = v {
                if ctx.read_string(s).is_some_and(|t| !t.is_empty()) {
                    return Ok(Some(v));
                }
            }
            return Ok(Some(Value::Object(Some(ctx.create_string("127.0.0.1")))));
        }
        let nf = ctx.object_num_fields(this);
        if nf > 1 {
            let addr = ctx.get_field(this, 1);
            if let Value::Object(Some(_)) = addr {
                return Ok(Some(addr));
            }
        }
        let s = ctx.create_string("127.0.0.1");
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getByName, getLoopbackAddress, getLocalHost — registered in lib.rs
    // with real DNS resolution; do NOT re-register here as it would shadow them.
    r.register(ia, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Real-JDK `InetAddress.toString()` => `hostName + "/" + getHostAddress()`,
        // and the right-hand half carries the IPv6 scope suffix.
        let host = match crate::net_phase_e::inet_addr_resolve(ctx, this) {
            Some((h, _)) => h,
            None => match ctx.get_field(this, 0) {
                Value::Object(Some(h)) => ctx.read_string(h).unwrap_or_default(),
                _ => String::new(),
            },
        };
        let ip = crate::net_phase_e::inet_addr_scoped_text(ctx, this, "");
        let s = ctx.create_string(&format!("{host}/{ip}"));
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);
    }

// ---------------------------------------------------------------------------
// Connection settings on the synthetic HttpURLConnection carrier.
//
// `net_phase_e`'s `URL.openConnection` allocates the carrier with 16 slots but
// its layout only uses 0..=9 (url/method/code/fd/reqHeaders/respHeaders/body/
// doInput/doOutput/connected), so 10..=15 are free. 10 and 11 deliberately
// match `http_url_connection.rs`'s HUC_CONNECT_TIMEOUT / HUC_READ_TIMEOUT, so
// the two synthetic HUC layouts in the tree agree at least on the timeouts.
// ---------------------------------------------------------------------------
pub const P54_HUC_CONNECT_TIMEOUT: usize = 10;
pub const P54_HUC_READ_TIMEOUT: usize = 11;
/// 1 = the flag was explicitly turned OFF. An unwritten slot decodes as
/// `Int(0)` (VTAG_INT is the zero tag), so both booleans are stored NEGATED —
/// that keeps the JDK default (`true`) for a carrier nobody configured, which
/// a straight 0/1 encoding could not express.
pub const P54_HUC_NO_REDIRECTS: usize = 12;
pub const P54_HUC_NO_CACHES: usize = 13;
/// `Long` = the configured streaming length / chunk size. Anything else (an
/// unwritten `Int(0)` slot) means "not set", which is how the two modes detect
/// each other the way `URLConnection`'s `-1` sentinels do.
pub const P54_HUC_FIXED_LENGTH: usize = 14;
pub const P54_HUC_CHUNK_LENGTH: usize = 15;

/// Slot write guarded on the receiver's width: `java/net/HttpURLConnection`
/// carries two different synthetic layouts in this tree (this file's 16-slot
/// one and `http_url_connection.rs`'s 12-slot one), so a narrower carrier must
/// drop the write rather than run off the end of the object.
pub fn p54_huc_set_slot(ctx: &mut dyn NativeContext, this: ObjectRef, slot: usize, value: Value) {
    if ctx.object_num_fields(this) > slot {
        ctx.set_field(this, slot, value);
    }
}

pub fn p54_huc_get_slot(ctx: &dyn NativeContext, this: ObjectRef, slot: usize) -> Value {
    if ctx.object_num_fields(this) > slot {
        ctx.get_field(this, slot)
    } else {
        Value::Int(0)
    }
}

/// `setFixedLengthStreamingMode(int|long)` — record the promised body length
/// and raise the two exceptions the method specifies. The request path still
/// buffers the body and derives `Content-Length` from it (same trade-off
/// `http_url_connection.rs` documents for its own carrier), so the recorded
/// length only has to be observable and mutually exclusive with the chunked
/// setting. The "already connected" `IllegalStateException` is deliberately
/// NOT raised: the connected flag lives at slot 9 in this layout but means
/// `instanceFollowRedirects` in the other one, so the check cannot be made
/// safely from a native registered on the shared class.
pub fn p54_huc_set_fixed_length_streaming_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let length = match args.get(1) {
        Some(Value::Int(v)) => *v as i64,
        Some(Value::Long(v)) => *v,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "invalid content length".to_string(),
            }
            .into())
        }
    };
    if length < 0 {
        return Err(RuntimeError::IllegalArgumentException {
            message: "invalid content length".to_string(),        }
        .into());
    }
    if matches!(
        p54_huc_get_slot(ctx, this, P54_HUC_CHUNK_LENGTH),
        Value::Long(_)
    ) {
        return Err(RuntimeError::IllegalStateException {
            message: "Chunked encoding streaming mode set".to_string(),        }
        .into());
    }
    p54_huc_set_slot(ctx, this, P54_HUC_FIXED_LENGTH, Value::Long(length));
    Ok(None)
}

/// `setChunkedStreamingMode(int)` — the mirror image of the above.
pub fn p54_huc_set_chunked_streaming_mode(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let chunk = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
    if matches!(
        p54_huc_get_slot(ctx, this, P54_HUC_FIXED_LENGTH),
        Value::Long(_)
    ) {
        return Err(RuntimeError::IllegalStateException {
            message: "Fixed length streaming mode set".to_string(),        }
        .into());
    }
    p54_huc_set_slot(ctx, this, P54_HUC_CHUNK_LENGTH, Value::Long(chunk as i64));
    Ok(None)
}

/// Pins `this` across [`p54_huc_do_request_body`] and hands the refreshed reference back.
///
/// The receiver is `&mut` on purpose. The body ALLOCATES and returns no
/// reference, so a moving collector could relocate `this` inside the call and
/// every caller was left holding a pre-move address -- the shape
/// `WORKER-5-NOTE-10` traced `TreeMap.size()` returning 0 to. `&mut` makes
/// forgetting the refresh a COMPILE ERROR instead of an audit finding.
pub fn p54_huc_do_request(
    ctx: &mut dyn NativeContext,
    this: &mut ObjectRef,
) -> Result<(), MethodCallFailed> {
    let w5_pin = ctx.pin_native_root(*this);
    let w5_out = p54_huc_do_request_body(ctx, *this);
    *this = ctx.read_native_pin(w5_pin, *this);
    ctx.unpin_native_roots(w5_pin);
    w5_out
}

