#[allow(unused_imports)]
use super::*;

// ===========================================================================
// RE.2 — java.net.ServerSocket
// ===========================================================================

fn re2_accept_into(
    ctx: &mut dyn NativeContext,
    listener_id: i32,
    mut target: ObjectRef,
    timeout_ms: i32,
) -> MethodCallResult {
    if listener_id < 0 {
        return Err(ioex("ServerSocket not bound"));
    }
    // gc-common w16-a: as in `re1_connect_socket`. Allocates nothing.
    net_release_deferred(ctx);
    // `target` is a Socket allocated by the enclosing ServerSocket.accept
    // native, not a Java-frame local or a safe_native_call argument. This method
    // can park in the poll loop below, where GC will scan only the deposited
    // blocked-thread snapshot. Keep the Socket in native_pin_roots for the whole
    // native so that snapshot includes it and any moving-GC fixup can be read
    // back before field writes and before returning it to Java.
    let target_pin = ctx.pin_native_root(target);
    // Poll-based accept. We deliberately do NOT block directly on a
    // `try_clone()`'d listener handle for the no-timeout case: on Windows,
    // closing one duplicated socket handle does not unblock a thread blocked in
    // `accept()` on another duplicate, so a `ServerSocket.close()` that drops
    // the registry's listener could never interrupt a blocked accept. okhttp's
    // MockWebServer relies on exactly that interruption — its accept runs on a
    // TaskRunner queue and `close()` waits up to 5 s for that queue to drain,
    // then throws `AssertionError: Gave up waiting for queue to shut down`
    // (it polluted every Spring HTTP-client test teardown).
    //
    // Instead we set the listener non-blocking and poll it, re-checking the
    // registry each iteration. When `close()` removes the listener (see
    // `re2_server_socket_close`), the next poll observes its absence and we
    // throw `SocketException("Socket closed")`, exactly as HotSpot's blocking
    // accept does on a closed ServerSocket. Each iteration holds the registry
    // lock only for the non-blocking accept syscall (never across a blocking
    // call), so the Hibernate-JTA bind/accept deadlock the old try_clone path
    // guarded against (registry lock held across a blocking accept) cannot
    // recur. `timeout_ms <= 0` means block indefinitely (until a connection
    // arrives or the socket is closed); `timeout_ms > 0` honours SO_TIMEOUT.
    enum AcceptOutcome {
        Accepted((TcpStream, SocketAddr)),
        Closed,
        TimedOut,
        Failed(std::io::Error),
    }
    let deadline = (timeout_ms > 0)
        .then(|| std::time::Instant::now() + Duration::from_millis(timeout_ms as u64));
    // The listener's raw handle, read under the registry lock on each pass, for
    // the kernel wait below. A hint only — see `cratonvm_native_api::net_wait`.
    let mut park_raw: Option<cratonvm_native_api::net_wait::RawSock> = None;
    let outcome = loop {
        {
            let mut reg = s2_registry().lock();
            match reg.listeners.get_mut(&listener_id) {
                Some(listener) => {
                    // Idempotent + cheap; keeps the socket pollable.
                    listener.set_nonblocking(true).ok();
                    park_raw = Some(cratonvm_native_api::net_wait::raw_sock(&*listener));
                    match listener.accept() {
                        Ok(pair) => break AcceptOutcome::Accepted(pair),
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                        Err(e) => break AcceptOutcome::Failed(e),
                    }
                }
                // Listener gone from the registry => another thread called
                // ServerSocket.close(). Mirror HotSpot: throw SocketException.
                None => break AcceptOutcome::Closed,
            }
        }
        if let Some(dl) = deadline {
            if std::time::Instant::now() >= dl {
                break AcceptOutcome::TimedOut;
            }
        }
        // GC-barrier safepoint gap: unlike every other blocking-native call in
        // this codebase (`Thread.sleep`, socket read/write), this poll-sleep
        // ran unmarked. A no-timeout `ServerSocket.accept()` (timeout_ms <= 0,
        // `deadline` is `None`) loops here indefinitely once no new
        // connections are pending, staying counted in the GC barrier's
        // `expected` set forever — it never reaches an interpreter safepoint
        // (it's native Rust code, not JIT either, so the cross-thread JIT
        // takeover can't rescue it), so any STW GC/JIT-takeover initiated
        // while this thread has no pending connection deadlocks permanently
        // (`stw-census` showed `pending=1 taken=0` unable to move). Mark each
        // poll-sleep slice as a blocked region, exactly like `Thread.sleep`'s
        // pump loop.
        //
        // `target` is a raw `ObjectRef` local held across the whole loop (it
        // is written to after a connection is accepted) — use the `_refs`
        // end-region variant so a moving GC that runs while we're blocked
        // rewrites it, same as `re1_socket_read_stream`'s `buf`.
        let mut blocked_refs = [Value::Object(Some(ctx.read_native_pin(target_pin, target)))];
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        // A kernel wait for a pending connection, bounded by the same 10 ms, so
        // the registry re-check cadence is unchanged but a connection arriving
        // mid-wait is taken at once rather than after the rest of a fixed sleep.
        match park_raw {
            Some(raw) if cratonvm_native_api::net_wait::event_waits_enabled() => {
                cratonvm_native_api::net_wait::wait_readable_raw(raw, 10);
            }
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
        ctx.end_blocking_region_refs(&mut blocked_refs);
        target = match blocked_refs[0] {
            Value::Object(Some(o)) => o,
            _ => target,
        };
        target = ctx.read_native_pin(target_pin, target);
    };

    // Restore blocking mode on the shared listener if it survived, so later
    // accept/getLocalSocketAddress calls see the conventional state.
    {
        let mut reg = s2_registry().lock();
        if let Some(l) = reg.listeners.get_mut(&listener_id) {
            l.set_nonblocking(false).ok();
        }
    }

    let (stream, peer) = match outcome {
        AcceptOutcome::Accepted(pair) => pair,
        AcceptOutcome::Closed => {
            // A real, catchable java.net.SocketException so MockWebServer's
            // `catch (SocketException)` accept loop terminates cleanly (and its
            // okhttp TaskRunner queue goes idle) instead of timing out.
            let jmsg = ctx.create_string("Socket closed");
            return Err(cratonvm_native_api::throwable_builder::throw_new(
                ctx,
                "java/net/SocketException",
                "(Ljava/lang/String;)V",
                &[Value::Object(Some(jmsg))],
                || ioex("ServerSocket.accept: Socket closed"),
            ));
        }
        AcceptOutcome::TimedOut => {
            // Concrete `java.net.SocketTimeoutException`, which is what HotSpot
            // throws when SO_TIMEOUT expires. An accept loop that catches the
            // timeout to keep polling and catches plain IOException to shut
            // down does the exact opposite of what it should if this is a bare
            // IOException.
            return Err(RuntimeError::SocketTimeoutException {
                message: "Accept timed out".into(),
            }
            .into());
        }
        AcceptOutcome::Failed(e) => {
            return Err(ioex(format!("ServerSocket.accept failed: {e}")));
        }
    };
    // Force the accepted stream to blocking so a server-side read() actually
    // waits for data (and is woken by it) instead of returning WouldBlock or
    // never signalling. (BUG-04)
    let _ = stream.set_nonblocking(false);
    let peer_port = peer.port() as i32;
    let peer_ip = peer.ip().to_string();
    let local_port = stream.local_addr().map(|a| a.port() as i32).unwrap_or(0);
    if crate::nbflags().dbg_sock {
        eprintln!(
            "[dbg-sock] accept: peer={peer_ip}:{peer_port} local_port={local_port} nonblocking_reset"
        );
    }
    // gc-common w14-a: the accepted stream inherits the listener's owning VM,
    // so that VM's teardown (`servlet::forget_vm_s2_sockets`) closes it.
    let stream_id = crate::servlet::s2_alloc_accepted_stream(listener_id, stream);
    // GC-safety: `target` is a raw ObjectRef local that survived the poll
    // loop above (already fixed via blocked_refs there), but this is a
    // SEPARATE, later hazard -- `create_string` is a re-entrant, allocating
    // call (it can allocate the String object and trigger a GC), and nothing
    // protected `target` across it. A moving GC here relocates/reclaims
    // `target` before the very next line's `set_field` uses it, corrupting
    // the just-accepted Socket (observed: TestJNDIRealmIntegration's "LDAP
    // Listener Thread" — a plain `Socket s = accept()` local, stored and
    // read back across nothing but an unconditional `goto`, going stale;
    // `Socket.getInetAddress()` then read a zeroed field and returned null).
    // Pin it exactly like every other raw-ObjectRef-across-a-reentrant-call
    // site in this codebase.
    let target = ctx.read_native_pin(target_pin, target);
    sock_set_owned(ctx, target, |s| {
        s.host = peer_ip.clone();
        s.port = peer_port;
        s.local_port = local_port;
        s.closed = 0;
        s.stream_id = stream_id;
        // gc-common w16-a: the accepted stream is this socket's to close if
        // the socket is dropped without `close()`.
        s.owns_stream = true;
    });
    // Leave `target_pin` live until `safe_native_call` installs
    // `native_pending_return` and truncates the native pin stack. Unpinning here
    // would recreate the native-return handoff window this path is protecting.
    Ok(Some(Value::Object(Some(target))))
}

pub fn re2_bind_listener(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    host: &str,
    port: i32,
    backlog: i32,
) -> MethodCallResult {
    // gc-common w18-f: close the listeners of `ServerSocket`s a collection
    // found dropped open before binding, so a port one of them held can be
    // bound again. Allocates nothing, so `this` stays current.
    net_release_deferred(ctx);
    // Same two refusals the real `ServerSocket.bind` makes before it touches
    // the impl. Without them a second bind silently replaced the listener
    // (leaking the first and changing `getLocalPort()` under the caller), and
    // binding a closed socket quietly succeeded.
    {
        let side = ss_get(ctx, this);
        if side.closed != 0 {
            return Err(socket_ex(ctx, "Socket is closed"));
        }
        if side.bound != 0 {
            return Err(socket_ex(ctx, "Already bound"));
        }
    }
    let ip = resolve_host(host)?;
    let addr = SocketAddr::new(ip, port.clamp(0, 65535) as u16);
    // GAP I6: `java.net.ServerSocket` binds a `TcpListener` directly rather
    // than through `fd_table`, so it needs the bare endpoint gate.
    crate::capability_gate::gate_network(&*ctx, &addr.to_string())?;
    let listener = re2_bind_with_pending_options(ctx, this, addr).map_err(|e| {
        // Must be a concrete `java.net.BindException`, not a generic
        // IOException with "BindException" as a text prefix — real code
        // (Spring Boot's `PortInUseException.throwIfPortBindingException`)
        // walks the cause chain with `instanceof BindException` and checks
        // the message for "in use"; a bare IOException is invisible to that
        // walk. See the sibling fix in native-io/src/{net,socket_channel}.rs.
        let message = match e.kind() {
            // The bare strerror text, as HotSpot's BindException carries it.
            std::io::ErrorKind::AddrInUse => "Address already in use".to_string(),
            std::io::ErrorKind::AddrNotAvailable => "Cannot assign requested address".to_string(),
            std::io::ErrorKind::PermissionDenied => "Permission denied".to_string(),
            _ => format!("{addr}: {e}"),
        };
        MethodCallFailed::from(RuntimeError::BindException { message })
    })?;
    let local_addr = listener.local_addr().ok();
    let actual_port = local_addr.map(|a| a.port() as i32).unwrap_or(port);
    // A wildcard bind reports the address the CALLER asked for, not the one the
    // dual-stack socket landed on. HotSpot's `new ServerSocket(0).getInetAddress()`
    // answers `0.0.0.0` even though the socket underneath is AF_INET6 with
    // `IPV6_V6ONLY` cleared (`probes/WildcardBindFamilyProbe.java` prints both),
    // because `ServerSocket` keeps the requested `InetAddress.anyLocalAddress()`.
    // Taking `local_addr()` here would answer `::` and change a getter that has
    // nothing to do with this fix.
    let actual_host = if ip.is_unspecified() {
        ip.to_string()
    } else {
        local_addr
            .map(|a| a.ip().to_string())
            .unwrap_or_else(|| ip.to_string())
    };
    let listener_id = s2_alloc_listener_for_vm(ctx.vm_identity(), listener);
    ss_set(ctx, this, |s| {
        s.port = actual_port;
        s.backlog = backlog.max(0);
        s.closed = 0;
        s.listener_id = listener_id;
        // Reached only for a plain ServerSocket: this surface's own
        // constructors, or the plain-bind handler native-io delegates to for a
        // receiver with no channel back-ref. Either way the state is ours.
        s.constructed = 1;
        s.bound = 1;
        s.host = actual_host.clone();
    });
    // Publish the actual bound port to the cross-crate identity-keyed registry. The
    // re2 side-table above is private to native-builtins, but the last-registered (and
    // therefore winning) `getLocalPort` native lives in the sibling native-io crate
    // (socket_channel `ss_wrapper_local_port`) and shadows ALL ServerSocket dispatch.
    // It cannot see our side-table, and an int written to object field 0 does NOT
    // round-trip (the real ServerSocket layout's low slots are reference-typed). The
    // shared native-api table (keyed by GC-stable identity hash) is what lets that
    // winner answer at all if it ever runs without the delegation hooks of
    // `cratonvm_native_api::plain_server_socket` installed.
    cratonvm_native_api::server_socket_ports::record_addr(
        ctx.vm_identity(),
        ctx.identity_hash_code(this),
        this,
        &actual_host,
        actual_port,
    );
    Ok(None)
}

/// Bind a listener with the socket options the caller set while it was still
/// unbound applied FIRST.
///
/// `TcpListener::bind` gives no window to configure the socket between
/// `socket()` and `bind()`, and SO_REUSEADDR is only meaningful in exactly that
/// window — so `new ServerSocket(); setReuseAddress(true); bind(addr)` (the one
/// ordering where the option changes anything, and the one JGroups/Netty use)
/// silently lost the request: the getter read the fresh listener back and
/// answered `false`. Build the socket by hand through `socket2` when there is a
/// retained option to apply, and fall back to the plain path otherwise.
///
/// A WILDCARD address additionally binds **dual-stack** — AF_INET6 with
/// `IPV6_V6ONLY` cleared — because that is what `new ServerSocket(0)` gives on
/// HotSpot: `probes/WildcardBindFamilyProbe.java` measures `::1` reaching a
/// HotSpot `new ServerSocket(0)` and being refused by CratonVM's. Same defect
/// and same fix as the `ServerSocketChannel` half in
/// `native-io/src/socket_channel.rs::bind_wildcard_listener`; an address that
/// NAMES a family still keeps that family.
pub fn re2_bind_with_pending_options(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    addr: SocketAddr,
) -> std::io::Result<TcpListener> {
    let side = ss_get(ctx, this);
    let wildcard = addr.ip().is_unspecified();
    if side.reuse_address < 0 && side.recv_buffer_size <= 0 {
        return if wildcard {
            cratonvm_native_api::fd_table::open_tcp_dual_stack_listener(addr.port(), side.backlog)
        } else {
            // Was `TcpListener::bind`, which listens at std's 128 regardless.
            cratonvm_native_api::net_wait::bind_tcp_listener(addr, side.backlog)
        };
    }
    let domain = match addr {
        _ if wildcard => socket2::Domain::IPV6,
        SocketAddr::V4(_) => socket2::Domain::IPV4,
        SocketAddr::V6(_) => socket2::Domain::IPV6,
    };
    let socket = socket2::Socket::new(domain, socket2::Type::STREAM, Some(socket2::Protocol::TCP))?;
    if side.reuse_address >= 0 {
        socket.set_reuse_address(side.reuse_address != 0)?;
    }
    if side.recv_buffer_size > 0 {
        socket.set_recv_buffer_size(side.recv_buffer_size as usize)?;
    }
    // The retained-options arm has to make the same dual-stack choice as the
    // plain one above, or `setReuseAddress(true)` would silently re-introduce
    // the v4-only listener this fix removes.
    let addr = if wildcard {
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, addr.port()))
    } else {
        addr
    };
    if wildcard {
        socket.set_only_v6(false)?;
    }
    socket.bind(&addr.into())?;
    // `backlog` here is the OS listen queue; the Java-level value is recorded
    // separately by the caller. The JDK rule is `backlog < 1 ? 50 : backlog`;
    // the `.max(50)` this replaces also raised a legitimate backlog of 10 to 50.
    cratonvm_native_api::net_wait::listen_jdk(&socket, side.backlog)?;
    Ok(socket.into())
}

/// Plain `java.net.ServerSocket.bind(SocketAddress[, int])` handler. Handles
/// both arities (backlog read from `args[2]` when present). Registered for both
/// descriptors below AND installed as the plain-`ServerSocket` bind handler
/// ([`cratonvm_native_api::plain_server_socket`]) so native-io's *winning*
/// `ss_wrapper_bind` (which shadows this registration) delegates the
/// no-channel-back-ref (plain `new ServerSocket()`) case back here instead of
/// no-opping — without which `new ServerSocket().bind(addr)` never bound a
/// listener and `getLocalPort()` stayed 0 (okhttp MockWebServer → port 0; BUG-04).
pub fn re2_server_socket_bind(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    // `bind(null)` is legal and means "an ephemeral port on the wildcard
    // address" (`ServerSocket.bind`: "if the address is null, then the system
    // will pick up an ephemeral port and a valid local address"). Refusing it
    // with an IOException broke every caller that binds late without caring
    // where.
    let (host, port) = match obj_arg(args, 1) {
        Ok(sa) => read_inet_socket_address(ctx, sa)?,
        Err(_) => ("0.0.0.0".to_string(), 0),
    };
    let backlog = args
        .get(2)
        .and_then(|v| v.as_int())
        .unwrap_or_else(|| ss_get(ctx, this).backlog);
    re2_bind_listener(ctx, this, &host, port, backlog)
}

/// `java.net.ServerSocket.close()` for the synthetic re2 surface. Drops the
/// listener from the shared `s2` registry (so the accept poll loop in
/// [`re2_accept_into`] observes the absence and throws `SocketException`,
/// matching HotSpot's blocked-accept-on-close) and clears the SO_TIMEOUT.
///
/// Also installed as the plain-`ServerSocket` close handler
/// ([`cratonvm_native_api::plain_server_socket`]) so native-io's *winning*
/// `ss_wrapper_close` (which shadows this registration) delegates the
/// no-channel-back-ref (plain `new ServerSocket()`) case back here instead of
/// no-opping. Without this, a plain `ServerSocket.close()` released nothing, so
/// the listener stayed registered and a blocked accept never woke: okhttp's
/// `MockWebServer.close()` waits up to 5 s for its accept TaskRunner queue to
/// drain on teardown, then throws
/// `AssertionError: Gave up waiting for queue to shut down` (BUG: it polluted
/// every Spring HTTP-client test teardown). Mirrors the BUG-04 plain-bind hook.
///
/// `bound`, `port` and `host` are deliberately NOT cleared: a closed
/// `ServerSocket` still reports `isBound() == true`, its former
/// `getLocalPort()` and its former `getLocalSocketAddress()` on HotSpot.
pub fn re2_server_socket_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let lid = ss_get(ctx, this).listener_id;
    if lid >= 0 {
        // Dropped (closing the descriptor) after the registry lock is
        // released: the close is a syscall (gc-common w18-f).
        let listener = s2_registry().lock().listeners.remove(&lid);
        drop(listener);
    }
    ss_set(ctx, this, |s| {
        s.closed = 1;
        s.listener_id = -1;
    });
    cratonvm_native_api::server_socket_ports::remove(ctx.identity_hash_code(this), this);
    Ok(None)
}

pub fn register_re2_server_socket(r: &mut NativeMethodRegistry) {
    // NIO-SERVER-SOCKET (route 1): skip the synthetic java.net.ServerSocket
    // surface so real bytecode drives sun/nio/ch/Net. See
    // register_phase53_socket_stubs.
    if crate::vmflags().io.real_net_sockets {
        return;
    }
    // Install the plain-`ServerSocket` handler set for native-io's winning
    // `ss_wrapper_*` natives to delegate to (BUG-04). Every method native-io
    // shadows is covered: answering some of them out of the
    // `server_socket_ports` side table instead left `isClosed()` stuck at
    // false, `isBound()` reverting after close, and `getLocalPort()` reporting
    // 0 rather than -1 while unbound. Done unconditionally here — the
    // early-return above is the REAL_NET_SOCKETS path, where native-io also
    // defers to real bytecode and nothing consults this.
    cratonvm_native_api::plain_server_socket::set(
        cratonvm_native_api::plain_server_socket::PlainServerSocketOps {
            bind: re2_server_socket_bind,
            close: re2_server_socket_close,
            local_port: re2_server_socket_local_port,
            local_socket_address: re2_server_socket_local_address,
            is_bound: re2_server_socket_is_bound,
            is_closed: re2_server_socket_is_closed,
        },
    );
    let ss = "java/net/ServerSocket";

    r.register(ss, "<init>", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // This native bypasses ServerSocket's field initializers.  Preserve
        // the real object's synchronization invariant before its bytecode
        // options path reaches getImpl().
        let this = re1_init_socket_locks(ctx, this)?;
        ss_set(ctx, this, |s| {
            s.port = -1;
            s.backlog = 50;
            s.closed = 0;
            s.listener_id = -1;
            s.constructed = 1;
            s.bound = 0;        });
        Ok(None)
    }, NativeKind::SyntheticStub);

    r.register(ss, "<init>", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let this = re1_init_socket_locks(ctx, this)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        re2_bind_listener(ctx, this, "0.0.0.0", port, 50)
    }, NativeKind::SyntheticStub);

    r.register(ss, "getLocalPort", "()I", re2_server_socket_local_port, NativeKind::SyntheticStub);

    r.register(ss, "<init>", "(II)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let this = re1_init_socket_locks(ctx, this)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        re2_bind_listener(ctx, this, "0.0.0.0", port, backlog)
    }, NativeKind::SyntheticStub);

    r.register(ss, "<init>", "(IILjava/net/InetAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let this = re1_init_socket_locks(ctx, this)?;
        let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        let backlog = args.get(2).and_then(|v| v.as_int()).unwrap_or(50);
        let host = match args.get(3) {
            Some(Value::Object(Some(ia))) => {
                inet_addr_field_string_or(ctx, *ia, IA_ADDR, "0.0.0.0")
            }
            _ => "0.0.0.0".to_string(),
        };
        let r = re2_bind_listener(ctx, this, &host, port, backlog);
        r
    }, NativeKind::SyntheticStub);

    r.register(
        ss,
        "bind",
        "(Ljava/net/SocketAddress;)V",
        re2_server_socket_bind, NativeKind::SyntheticStub,
    );
    r.register(
        ss,
        "bind",
        "(Ljava/net/SocketAddress;I)V",
        re2_server_socket_bind, NativeKind::SyntheticStub,
    );

    r.register(ss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let s = ss_get(ctx, this);
        if s.closed != 0 {
            return Err(socket_ex(ctx, "Socket is closed"));
        }
        // A `ServerSocketChannel.socket()` adapter is also a
        // `java.net.ServerSocket` and also lands here: native-io wraps six
        // ServerSocket methods for the channel case, but not `accept`. Its
        // listener lives in native-io's channel registry, so nothing in this
        // surface's state describes it — `listener_id` is -1 and `bound` is 0,
        // which produced `IOException: ServerSocket not bound` where HotSpot
        // accepts (or times out). Hand it back to the crate that owns the
        // channel, carrying the SO_TIMEOUT this surface DOES hold for it
        // (`setSoTimeout` is not wrapped either, so the value was stored here).
        //
        // Only for receivers this surface did not construct: a plain
        // ServerSocket is never channel-backed, so the hook would just be a
        // wasted lookup for it.
        if s.constructed == 0 {
            if let Some(accept_on_channel) =
                cratonvm_native_api::plain_server_socket::channel_backed_accept()
            {
                if let Some(result) = accept_on_channel(ctx, this, s.so_timeout) {
                    return result;
                }
            }
        }
        // Only refuse for a receiver this surface constructed — for anything
        // else `bound` says nothing (see above).
        if s.constructed != 0 && s.bound == 0 {
            // HotSpot: `SocketException: Socket is not bound`, not an
            // IOException — callers catch the subtype.
            return Err(socket_ex(ctx, "Socket is not bound"));
        }
        let lid = s.listener_id;
        let timeout_ms = s.so_timeout;
        let sock = try_alloc_concurrent_synthetic(ctx, "java/net/Socket", 5)?;
        sock_set_owned(ctx, sock, |x| {
            x.port = 0;
            x.local_port = 0;
            x.closed = 0;
            x.stream_id = -1;
        });
        let sock = re1_init_socket_locks(ctx, sock)?;
        re2_accept_into(ctx, lid, sock, timeout_ms)
    }, NativeKind::SyntheticStub);

    // SO_TIMEOUT is held per RECEIVER, not per listener id: `setSoTimeout` is
    // legal on an UNBOUND ServerSocket (`new ServerSocket(); setSoTimeout(ms);
    // bind(addr)`), where there is no listener id to key it by. Keying it by
    // one meant that ordering silently discarded the timeout and the later
    // `accept()` blocked forever.
    r.register(ss, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ms = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if ms < 0 {
            return Err(iae(format!("negative SO_TIMEOUT: {ms}")));
        }
        ss_set(ctx, this, |s| s.so_timeout = ms);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ss, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ss_get(ctx, this).so_timeout)))
    }, NativeKind::SyntheticStub);

    // setReuseAddress / getReuseAddress: the synthetic `ServerSocket` has no
    // real `SocketImpl`, so the JDK bytecode for these (`getImpl().setOption(
    // SO_REUSEADDR, …)`) would NPE — `getImpl()` does `synchronized
    // (socketLock)` on a `socketLock` the synthetic `<init>` never initialises
    // (`NullPointerException: monitorenter in ServerSocket.getImpl`). This bites
    // WildFly's managed-container port check (`isPortAvailable` →
    // `new ServerSocket(port)` then `setReuseAddress(true)`). They used to be
    // pure no-ops with a hardcoded `true` getter, which lied twice: on Windows
    // `TcpListener::bind` does NOT set SO_REUSEADDR, so `getReuseAddress()`
    // claimed an option the listener did not have, and a caller that turned the
    // option OFF (JGroups and Netty both do, to make a port conflict fail fast
    // instead of silently sharing) was ignored and still read back `true`.
    //
    // Apply it to the real `TcpListener` whenever one exists, and always retain
    // the requested value: Java permits the call on an UNBOUND ServerSocket
    // (that is the only ordering where SO_REUSEADDR changes bind behaviour) and
    // there is no OS handle to hold it until `re2_bind_listener` runs.
    r.register(ss, "setReuseAddress", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        let lid = ss_get(ctx, this).listener_id;
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(l) = reg.listeners.get(&lid) {
                socket2::SockRef::from(l)
                    .set_reuse_address(on)
                    .map_err(|e| ioex(format!("setReuseAddress failed: {e}")))?;
            }
        }
        ss_set(ctx, this, |s| s.reuse_address = i32::from(on));
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ss, "getReuseAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let side = ss_get(ctx, this);
        if side.listener_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(l) = reg.listeners.get(&side.listener_id) {
                if let Ok(on) = socket2::SockRef::from(l).reuse_address() {
                    return Ok(Some(Value::Int(i32::from(on))));
                }
            }
        }
        // Unbound (or the OS refused the query): report what the caller last
        // asked for. `1` only when nobody ever asked — the historical answer,
        // and the platform default for a bound `ServerSocket` on Unix.
        Ok(Some(Value::Int(if side.reuse_address < 0 {
            1
        } else {
            side.reuse_address
        })))
    }, NativeKind::SyntheticStub);

    // The RE2 constructors own listener state outside the real ServerSocket
    // implementation.  JGroups configures this option before bind, where it
    // must be accepted without entering the real getImpl() bytecode path — but
    // "accepted" used to mean "discarded", with the getter answering a
    // hardcoded 8192 regardless. The accept-side receive buffer is inherited by
    // every accepted connection, so silently dropping a caller's sizing is a
    // throughput setting that goes missing with no diagnostic at all.
    r.register(ss, "setReceiveBufferSize", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let size = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        if size <= 0 {
            return Err(iae(format!("negative receive buffer size: {size}")));
        }
        let lid = ss_get(ctx, this).listener_id;
        if lid >= 0 {
            let reg = s2_registry().lock();
            if let Some(l) = reg.listeners.get(&lid) {
                socket2::SockRef::from(l)
                    .set_recv_buffer_size(size as usize)
                    .map_err(|e| ioex(format!("setReceiveBufferSize failed: {e}")))?;
            }
        }
        ss_set(ctx, this, |s| s.recv_buffer_size = size);
        Ok(None)
    }, NativeKind::SyntheticStub);
    r.register(ss, "getReceiveBufferSize", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let side = ss_get(ctx, this);
        if side.listener_id >= 0 {
            let reg = s2_registry().lock();
            if let Some(l) = reg.listeners.get(&side.listener_id) {
                // Linux reports back roughly twice what was requested (kernel
                // bookkeeping overhead). Real `ServerSocket.getReceiveBufferSize`
                // has exactly the same behaviour, so pass it through unmodified.
                if let Ok(sz) = socket2::SockRef::from(l).recv_buffer_size() {
                    return Ok(Some(Value::Int(sz as i32)));
                }
            }
        }
        Ok(Some(Value::Int(if side.recv_buffer_size <= 0 {
            8192
        } else {
            side.recv_buffer_size
        })))
    }, NativeKind::SyntheticStub);
    r.register(ss, "close", "()V", re2_server_socket_close, NativeKind::SyntheticStub);

    r.register(ss, "isBound", "()Z", re2_server_socket_is_bound, NativeKind::SyntheticStub);
    r.register(ss, "isClosed", "()Z", re2_server_socket_is_closed, NativeKind::SyntheticStub);

    r.register(
        ss,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        re2_server_socket_local_address, NativeKind::SyntheticStub,
    );

    // `getInetAddress()` — the bound local address. This was previously
    // unregistered, so it fell through to the real `ServerSocket.getInetAddress`
    // bytecode (`if (!isBound()) return null; …`) which returns **null** for the
    // synthetic socket. Narayana's `TxControl.<clinit>` does
    // `serverSocket.getInetAddress().getHostAddress()`, so that null NPEs and
    // aborts every JTA-platform test (HIB-DEV-02). Resolve the bound IP from the
    // shared listener registry — checking both the re2 side-table and the
    // object-field listener id used by the phase-53 ServerSocket ctors (the two
    // synthetic surfaces coexist; see `reference_server_socket_gap`) — and fall
    // back to the wildcard address so the caller gets a usable, non-null
    // InetAddress (matching `new ServerSocket(port).getInetAddress()` == 0.0.0.0
    // on HotSpot) instead of an NPE. That fallback applies only to a socket
    // that IS bound: on an unbound one the real method returns null, and
    // handing back 0.0.0.0 there is a different lie.
    r.register(
        ss,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let side = ss_get(ctx, this);
            let mut lid = side.listener_id;
            // Only read the object field for a receiver this surface has never
            // touched (a phase-53-constructed ServerSocket, which really does
            // keep its listener id in slot 3). On a real-layout ServerSocket
            // that slot is some unrelated JDK field, so reading it
            // unconditionally invented a non-negative "listener id" for a
            // freshly constructed socket — and `getInetAddress()` then answered
            // 0.0.0.0 where the JDK returns null. This is the exact layout
            // collision the side table exists to avoid.
            if lid < 0 && !ss_tracked(ctx, this) {
                lid = ctx.get_field(this, SS_LISTENER_ID).as_int().unwrap_or(-1);
            }
            if side.bound == 0 && lid < 0 {
                return Ok(Some(Value::Object(None)));
            }
            let ip = if lid >= 0 {
                let reg = s2_registry().lock();
                reg.listeners
                    .get(&lid)
                    .and_then(|l| l.local_addr().ok())
                    .map(|a| a.ip().to_string())
            } else {
                None
            };
            // THE RECORDED BIND ADDRESS OUTRANKS THE LISTENER'S.
            //
            // `side.host` is what the bind handler stored, and it already
            // applies the rule this getter needs: a wildcard bind records the
            // address the CALLER asked for, because the socket underneath is
            // AF_INET6 with `IPV6_V6ONLY` cleared and its `local_addr()` says
            // `::` whichever wildcard was requested. Reading the listener first
            // threw that away and answered `::` for `new ServerSocket(0)`,
            // where HotSpot -- on the same machine, equally dual-stack --
            // answers `0.0.0.0`.
            //
            // The listener remains the fallback for a receiver this surface
            // never bound (a phase-53-constructed ServerSocket, reached through
            // the object-field listener id above), which has no recorded host.
            let ip = (!side.host.is_empty())
                .then(|| side.host.clone())
                .or(ip)
                .unwrap_or_else(|| "0.0.0.0".to_string());
            // `ServerSocket.getInetAddress()` hands back the very `InetAddress`
            // that was passed to `bind`, so whether it carries a name is
            // inherited from THAT object rather than decided here. This surface
            // only retained the address as text, so reconstruct the one
            // distinction that is observable: a wildcard bind can only have
            // come from `InetAddress.anyLocalAddress()`, which IS named
            // (HotSpot prints `0.0.0.0/0.0.0.0`), whereas an explicit bind
            // address came from `getByName`/`getByAddress` and is not.
            //
            // This is NOT "unspecified implies named" in general:
            // `DatagramSocket.getLocalAddress()` on a wildcard-bound socket
            // prints `/0:0:0:0:0:0:0:0`, because that one is rebuilt from the
            // fd through `getByAddress` rather than remembered. Those sites
            // stay on `alloc_inet_address_unnamed`.
            let is_wildcard = ip
                .parse::<IpAddr>()
                .map(|parsed| parsed.is_unspecified())
                .unwrap_or(false);
            let ia = if is_wildcard {
                alloc_inet_address(ctx, &ip, &ip)            } else {
                alloc_inet_address_unnamed(ctx, &ip)            }?;
            Ok(Some(Value::Object(Some(ia))))
        }, NativeKind::SyntheticStub,
    );
}

// ---------------------------------------------------------------------------
// Plain-`ServerSocket` accessors.
//
// Named (not inline closures) because each is registered here AND installed in
// `cratonvm_native_api::plain_server_socket` for native-io's winning
// `ss_wrapper_*` natives to delegate to — one implementation, one set of
// answers, whichever crate's registration wins.
// ---------------------------------------------------------------------------

/// `getLocalPort()` — the bound port, retained after `close()`; `-1` when the
/// socket was never bound (`ServerSocket.getLocalPort`: "returns -1 if the
/// socket is not bound yet").
pub fn re2_server_socket_local_port(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let side = ss_get(ctx, this);
    Ok(Some(Value::Int(if side.bound == 0 {
        -1
    } else {
        side.port
    })))
}

/// `getLocalSocketAddress()` — `null` only while unbound. A closed socket still
/// reports the address it was bound to, as on HotSpot.
pub fn re2_server_socket_local_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let side = ss_get(ctx, this);
    if side.bound == 0 || side.port <= 0 {
        return Ok(Some(Value::Object(None)));
    }
    // Prefer the live listener (authoritative), then the address recorded at
    // bind time (the only source once the listener is gone).
    let live = (side.listener_id >= 0)
        .then(|| {
            let reg = s2_registry().lock();
            reg.listeners
                .get(&side.listener_id)
                .and_then(|l| l.local_addr().ok())
                .map(|a| (a.ip().to_string(), a.port() as i32))
        })
        .flatten();
    // Same split as `getInetAddress`: the ADDRESS comes from what was bound,
    // the PORT from the live listener. They have different authorities -- an
    // ephemeral port is only knowable from the socket, while the wildcard
    // family is only knowable from the request, since a dual-stack listener
    // reports `::` for either one.
    let (live_ip, live_port) = match live {
        Some((ip, port)) => (Some(ip), Some(port)),
        None => (None, None),
    };
    let ip = (!side.host.is_empty())
        .then(|| side.host.clone())
        .or(live_ip)
        .unwrap_or_else(|| "0.0.0.0".to_string());
    let port = live_port.unwrap_or(side.port);
    Ok(Some(Value::Object(Some(
        alloc_inet_socket_address_resolved(ctx, &ip, &ip, port)?,
    ))))
}

/// `isBound()` — true once a bind has succeeded, and true forever after,
/// including past `close()` (JDK: "will continue to return true after the
/// socket is closed").
pub fn re2_server_socket_is_bound(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(ss_get(ctx, this).bound)))
}

/// `isClosed()`.
pub fn re2_server_socket_is_closed(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    Ok(Some(Value::Int(ss_get(ctx, this).closed)))
}

// ===========================================================================
// RE.3 — java.net.InetAddress
// ===========================================================================

pub fn register_re3_inet_address(r: &mut NativeMethodRegistry) {
    let ia = "java/net/InetAddress";

    r.register(
        ia,
        "getByName",
        "(Ljava/lang/String;)Ljava/net/InetAddress;",
        |ctx, args| {
            let host = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let ip = resolve_host(&host)?;
            let name = if host.is_empty() {
                "localhost".to_string()
            } else {
                host
            };
            // `getByName(null)`/`getByName("")` mean the loopback and DO carry
            // the name "localhost". A numeric literal carries none — HotSpot's
            // `getByName("127.0.0.1").toString()` is `/127.0.0.1` — which is
            // what `alloc_inet_address_for_input` decides.
            //
            // `resolve_host` answers with a bare `IpAddr`, which cannot carry
            // an IPv6 `%scope`; take it back off the input text so
            // `getByName("fe80::1%1").getScopeId()` is 1 rather than 0.
            let ip_text = match host_input_scope(&name) {
                Some(scope) if ip.is_ipv6() => format!("{ip}%{scope}"),
                _ => ip.to_string(),
            };
            let obj = alloc_inet_address_for_input(ctx, &name, &ip_text)?;
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        ia,
        "getAllByName",
        "(Ljava/lang/String;)[Ljava/net/InetAddress;",
        |ctx, args| {
            let host = match args.first() {
                Some(Value::Object(Some(o))) => ctx.read_string(*o).unwrap_or_default(),
                _ => String::new(),
            };
            let lookup = if host.is_empty() || host == "localhost" {
                "localhost:0".to_string()
            } else {
                format!("{host}:0")
            };
            let mut addrs: Vec<String> = Vec::new();
            // Same literal screen as `getByName`; without it this entry point
            // answers for `0x7f.0.0.1` and reports a resolver failure in the
            // wrong words for `1::2::3`.
            if !(host.is_empty() || host == "localhost") {
                if let Some(ip) = literal_screen(&host)? {
                    addrs.push(ip.to_string());
                }
            }
            if addrs.is_empty() {
                match std::net::ToSocketAddrs::to_socket_addrs(&lookup.as_str()) {
                    Ok(iter) => {
                        for sa in iter {
                            addrs.push(sa.ip().to_string());
                        }
                    }
                    Err(e) => {
                        return Err(uhex(format!("{host}: {}", resolver_message(&e))));
                    }
                }
            }
            // Test servers bind their loopback listener on IPv4. Prefer that
            // address for localhost. Apache HttpClient5's multi-address retry
            // owns multiple connecting channels concurrently; on this runtime
            // it can leave the aggregate request pending after both loopback
            // refusals, whereas a single concrete route reports promptly.
            if host.is_empty() || host == "localhost" {
                addrs.sort_by_key(|ip| if ip.contains(':') { 1 } else { 0 });
                addrs.truncate(1);
            }
            if addrs.is_empty() {
                return Err(uhex(format!("{host}")));
            }
            // `InetAddress[]`, the declared type (i6-L2). A registered static
            // native can run before its own class is loaded (the native
            // `invokestatic` route, i1-L3), and `reflection_component_id`
            // does not load, so the first call of a run answered `Object[]`.
            // No heap reference is held yet, so loading here is safe.
            if ctx.class_id_by_name("java/net/InetAddress").is_none() {
                ctx.load_class("java/net/InetAddress")?;
            }
            let component = crate::lang_class::reflection_component_id(ctx, "java/net/InetAddress");
            let arr = ctx.new_ref_array(component, addrs.len());
            // Each element is an allocation that may initialise a class (a GC
            // point): keep the result array pinned and re-read it per store.
            let arr_pin = ctx.pin_native_root(arr);
            let name = if host.is_empty() {
                "localhost".to_string()
            } else {
                host
            };
            // Same rule as `getByName` above: `to_socket_addrs` cannot carry an
            // IPv6 `%scope`, so take it back off the input text.
            let scope = host_input_scope(&name).map(str::to_string);
            for (i, ip) in addrs.iter().enumerate() {
                let text = match scope.as_deref() {
                    Some(s) if ip.contains(':') => format!("{ip}%{s}"),
                    _ => ip.clone(),
                };
                // Same rule as `getByName` above: a literal keeps no name.
                let obj = match alloc_inet_address_for_input(ctx, &name, &text) {
                    Ok(obj) => obj,
                    Err(e) => {
                        ctx.unpin_native_roots(arr_pin);
                        return Err(e);
                    }
                };
                let arr = ctx.read_native_pin(arr_pin, arr);
                ctx.set_array_element(arr, i, Value::Object(Some(obj)));
            }
            let arr = ctx.read_native_pin(arr_pin, arr);
            ctx.unpin_native_roots(arr_pin);
            Ok(Some(Value::Object(Some(arr))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        ia,
        "getLoopbackAddress",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let obj = alloc_inet_address(ctx, "localhost", "127.0.0.1")?;
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );

    r.register(
        ia,
        "getLocalHost",
        "()Ljava/net/InetAddress;",
        |ctx, _args| {
            let hostname = hostname_string();
            let ip = resolve_host(&hostname)
                .map(|i| i.to_string())
                .unwrap_or_else(|_| "127.0.0.1".to_string());
            let obj = alloc_inet_address(ctx, &hostname, &ip)?;
            Ok(Some(Value::Object(Some(obj))))
        }, NativeKind::SyntheticStub,
    );

    r.register(ia, "getHostAddress", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(inet_addr_host_address_value(ctx, this)))
    }, NativeKind::SyntheticStub);
    r.register(ia, "getHostName", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(inet_addr_host_name_value(ctx, this)))
    }, NativeKind::SyntheticStub);
    r.register(
        ia,
        "getCanonicalHostName",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(inet_addr_host_name_value(ctx, this)))
        }, NativeKind::SyntheticStub,
    );
    r.register(ia, "getAddress", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
        let bytes: Vec<u8> = if let Ok(v4) = ip_str.parse::<Ipv4Addr>() {
            v4.octets().to_vec()
        } else if let Ok(v6) = ip_str.parse::<Ipv6Addr>() {
            v6.octets().to_vec()
        } else {
            vec![0u8; 4]
        };
        let arr = new_java_byte_array(ctx, &bytes);
        Ok(Some(Value::Object(Some(arr))))
    }, NativeKind::SyntheticStub);

    r.register(ia, "isLoopbackAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        let is_lb = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
        Ok(Some(Value::Int(if is_lb { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);
    r.register(ia, "isAnyLocalAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        let any = ip_str
            .parse::<IpAddr>()
            .map(|ip| match ip {
                IpAddr::V4(v) => v.is_unspecified(),
                IpAddr::V6(v) => v.is_unspecified(),
            })
            .unwrap_or(false);
        Ok(Some(Value::Int(if any { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);
    r.register(ia, "isMulticastAddress", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        let m = ip_str
            .parse::<IpAddr>()
            .map(|ip| ip.is_multicast())
            .unwrap_or(false);
        Ok(Some(Value::Int(if m { 1 } else { 0 })))
    }, NativeKind::SyntheticStub);

    r.register(
        ia,
        "getByAddress",
        "([B)Ljava/net/InetAddress;",
        native_inet_get_by_address, NativeKind::SyntheticStub,
    );

    // `equals`, `hashCode`, `toString` — registered for `InetAddress` and
    // both concrete subclasses below. Real-JDK implements these by reading
    // the `holder`; routing them through the layout-aware reader keeps a
    // CratonVM-synthesised mirror consistent with whatever the application
    // (e.g. Hazelcast's `DefaultAddressPicker`, which keys cluster members
    // by `InetAddress`) expects.
    register_inet_address_object_methods(r, ia);

    for cls in ["java/net/Inet4Address", "java/net/Inet6Address"] {
        r.register(
            cls,
            "getHostAddress",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                Ok(Some(inet_addr_host_address_value(ctx, this)))
            }, NativeKind::SyntheticStub,
        );
        // These CONCRETE-subclass registrations are the ones that actually
        // run: every mirror is an `Inet4Address`/`Inet6Address`, so they win
        // over the `java/net/InetAddress` pair registered above. Reading
        // `IA_HOST` raw answered `""` for an address carrying no hostName --
        // both pairs must share `inet_addr_host_name_value` or they silently
        // disagree depending on which class the receiver happens to be.
        r.register(cls, "getHostName", "()Ljava/lang/String;", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(inet_addr_host_name_value(ctx, this)))
        }, NativeKind::SyntheticStub);
        // NOT `inet_addr_host_name_value`: see `inet_address::canonical_host_name`
        // for the measured HotSpot split. `getHostName` gives back the name the
        // mirror was constructed with; `getCanonicalHostName` ignores it and
        // does its own reverse lookup, falling back to the textual address.
        r.register(
            cls,
            "getCanonicalHostName",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let text = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
                let canonical = match text.parse::<IpAddr>() {
                    Ok(ip) => crate::inet_address::canonical_host_name(&ip),
                    // No parsable address on the mirror: there is nothing to
                    // look up, so the constructed name is the only answer left
                    // and it is still better than an empty string.
                    Err(_) => return Ok(Some(inet_addr_host_name_value(ctx, this))),
                };
                Ok(Some(Value::Object(Some(ctx.create_string(&canonical)))))
            }, NativeKind::SyntheticStub,
        );
        r.register(cls, "getAddress", "()[B", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(inet_addr_address_bytes(
                ctx, this,
            )))))
        }, NativeKind::SyntheticStub);
        r.register(cls, "isLoopbackAddress", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(inet_addr_predicate(
                ctx,
                this,
                |ip| ip.is_loopback(),
            )))))
        }, NativeKind::SyntheticStub);
        r.register(cls, "isAnyLocalAddress", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(inet_addr_predicate(
                ctx,
                this,
                |ip| match ip {
                    IpAddr::V4(v) => v.is_unspecified(),
                    IpAddr::V6(v) => v.is_unspecified(),                },
            )))))
        }, NativeKind::SyntheticStub);
        r.register(cls, "isMulticastAddress", "()Z", |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Int(i32::from(inet_addr_predicate(
                ctx,
                this,
                |ip| ip.is_multicast(),
            )))))
        }, NativeKind::SyntheticStub);
        register_inet_address_object_methods(r, cls);    }}

/// Parse the `IA_ADDR` field of an InetAddress mirror and apply `pred`.
/// Used by the `isLoopbackAddress` / `isAnyLocalAddress` / `isMulticastAddress`
/// predicate natives shared across `InetAddress` and its subclasses.
pub fn inet_addr_predicate(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    pred: impl Fn(IpAddr) -> bool,
) -> bool {
    inet_addr_field_string_or(ctx, this, IA_ADDR, "")
        .parse::<IpAddr>()
        .map(pred)
        .unwrap_or(false)
}

/// Raw-bytes form of an InetAddress mirror's IP (4 bytes IPv4 / 16 IPv6).
pub fn inet_addr_address_bytes(ctx: &mut dyn NativeContext, this: ObjectRef) -> ObjectRef {
    let ip_str = inet_addr_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
    let bytes: Vec<u8> = if let Ok(v4) = ip_str.parse::<Ipv4Addr>() {
        v4.octets().to_vec()
    } else if let Ok(v6) = ip_str.parse::<Ipv6Addr>() {
        v6.octets().to_vec()
    } else {
        vec![0u8; 4]
    };
    new_java_byte_array(ctx, &bytes)
}

/// Register `equals(Object)`, `hashCode()`, `toString()` for an InetAddress
/// class. These mirror real-JDK semantics (equality by address bytes, hash
/// of the address int, `toString` = `"hostName/ipAddress"`) but read state
/// through the layout-aware [`inet_addr_resolve`] reader so they work on
/// CratonVM-synthesised mirrors whose host/IP lives in the side table.
pub fn register_inet_address_object_methods(r: &mut NativeMethodRegistry, cls: &'static str) {
    r.register(cls, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let host = inet_addr_field_string_or(ctx, this, IA_HOST, "");
        // Real-JDK `InetAddress.toString()` is
        // `Objects.toString(holder().getHostName(), "") + "/" + getHostAddress()`
        // — the right-hand half is `getHostAddress()`, so it carries the IPv6
        // scope suffix too.
        let ip = inet_addr_scoped_text(ctx, this, "");
        let s = ctx.create_string(&format!("{host}/{ip}"));
        Ok(Some(Value::Object(Some(s))))
    }, NativeKind::SyntheticStub);
    r.register(cls, "hashCode", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let ip = inet_addr_field_string_or(ctx, this, IA_ADDR, "0.0.0.0");
        // Real-JDK `Inet4Address.hashCode()` returns the packed address int.
        let h = match ip.parse::<IpAddr>() {
            Ok(IpAddr::V4(v4)) => i32::from_be_bytes(v4.octets()),
            // `Inet6Address.hashCode()` is a wrapping SUM of the four 4-byte
            // groups, each accumulated as `(component << 8) + ipaddress[i]`
            // over SIGNED bytes — not an XOR, and not over unsigned bytes.
            // Both differences are observable: for
            // `fe80:3030:3030:3030:3030:3030:3030:3031` the JDK answers
            // -1911504703 where the XOR-of-unsigned form answered something
            // else entirely, so an `Inet6Address` used as a HashMap key
            // hashed to a different bucket than an equal one obtained from
            // real-JDK bytecode.
            Ok(IpAddr::V6(v6)) => {
                let mut h = 0i32;
                for chunk in v6.octets().chunks(4) {
                    let mut component = 0i32;
                    for b in chunk {
                        component = component.wrapping_shl(8).wrapping_add(i32::from(*b as i8));
                    }
                    h = h.wrapping_add(component);
                }
                h
            }
            Err(_) => 0,
        };
        Ok(Some(Value::Int(h)))
    }, NativeKind::SyntheticStub);
    r.register(cls, "equals", "(Ljava/lang/Object;)Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let other = match args.get(1) {
            Some(Value::Object(Some(o))) => *o,
            _ => return Ok(Some(Value::Int(0))),
        };
        // Real-JDK `InetAddress.equals` compares the address bytes.
        let a = inet_addr_field_string_or(ctx, this, IA_ADDR, "");
        let b = inet_addr_field_string_or(ctx, other, IA_ADDR, "");
        let eq = !a.is_empty() && a.parse::<IpAddr>().ok() == b.parse::<IpAddr>().ok();
        Ok(Some(Value::Int(i32::from(eq))))
    }, NativeKind::SyntheticStub);
}

// ===========================================================================
// RE.4 — java.net.URL + HttpURLConnection
// ===========================================================================

pub const HUC_URL: usize = 0;
pub const HUC_METHOD: usize = 1;
pub const HUC_CODE: usize = 2;
pub const HUC_FD: usize = 3;
pub const HUC_REQ_HEADERS: usize = 4;
pub const HUC_RESP_HEADERS: usize = 5;
pub const HUC_BODY: usize = 6;
pub const HUC_DO_INPUT: usize = 7;
pub const HUC_DO_OUTPUT: usize = 8;
pub const HUC_CONNECTED: usize = 9;
// Synthetic JarURLConnection instances reserve the first ten carrier slots
// for URLConnection state. Keep the connection-owned JarFile immediately
// after them so repeated getJarFile() calls observe the same close state.
pub const HUC_JAR_FILE: usize = 10;

pub struct HttpResponse {
    pub status: i32,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    // The following four are unset (default) by `http_read_response` itself
    // and filled in by its callers, which are the only places with the
    // context (the request URL/method for this hop, the TLS connection, the
    // redirect chain) to populate them. See `re5_do_request`'s HttpResponse
    // registration gap fix: these back `uri()`, `request()`,
    // `previousResponse()` and `sslSession()`, the four (of five) real-response
    // accessors this struct did not used to carry enough information to
    // answer.
    /// The URL this specific hop was fetched from, set by
    /// `http_perform_request_with_timeout`. Empty for a response nobody has
    /// annotated yet.
    pub url: String,
    /// The HTTP method actually sent for this hop (redirects can rewrite it,
    /// e.g. 303 forces GET).
    pub method: String,
    /// The response this hop redirected FROM, if any — one link of the chain
    /// `HttpResponse.previousResponse()` walks. `None` for the first request
    /// of a redirect sequence, or when no redirect occurred at all.
    pub prev: Option<Box<HttpResponse>>,
    /// The negotiated TLS session for this hop, when it went over HTTPS
    /// through the rustls-backed exchange (`http_exchange_rustls`, i.e. a
    /// caller-supplied `SSLContext`). `None` for plain HTTP, and also for the
    /// native-tls-backed default-trust path (`http_exchange_tls`), which does
    /// not expose session introspection — `sslSession()` reports absent
    /// there rather than fabricate a cipher/protocol nobody measured.
    pub tls: Option<Re5TlsSessionInfo>,
}

impl Default for HttpResponse {
    fn default() -> Self {
        HttpResponse {
            status: 0,
            headers: Vec::new(),
            body: Vec::new(),
            url: String::new(),
            method: String::new(),
            prev: None,
            tls: None,
        }
    }
}

/// What `http_exchange_rustls` captures from a completed handshake, real
/// enough to back a genuine `javax.net.ssl.SSLSession` — the same information
/// `t27_tls::build_synthetic_ssl_session` reads off an `SSLEngine`, captured
/// here directly off the `rustls::ClientConnection` this file already drives.
pub struct Re5TlsSessionInfo {
    /// "TLSv1.3" / "TLSv1.2", JSSE's own null sentinel otherwise.
    pub protocol: String,
    /// JSSE cipher-suite name, JSSE's own null sentinel if none negotiated.
    pub cipher_suite: String,
    /// The peer's certificate chain, DER, leaf first.
    pub peer_certs_der: Vec<Vec<u8>>,
}

/// Parse an `http(s)://[userinfo@]host[:port][/path]` URL into
/// `(https, host, port, path, userinfo)`. The user-info (if any) is stripped
/// from the connect target / Host header and returned separately so callers
/// can turn it into preemptive `Authorization: Basic` credentials (the
/// real-JDK `HttpURLConnection` behaviour Spring's
/// `ResourceTests.useUserInfoToSetBasicAuth` relies on).
pub fn http_parse_url(url: &str) -> Result<(bool, String, u16, String, Option<String>), String> {
    let (scheme, rest) = if let Some(s) = url.strip_prefix("http://") {
        (false, s)
    } else if let Some(s) = url.strip_prefix("https://") {
        (true, s)
    } else {
        return Err(format!("unsupported URL: {url}"));
    };
    // An authority ends at the first path, query, or fragment delimiter. A
    // query-only target (for example `http://host:8080?trace=false`) must not
    // feed `8080?trace=false` to the port parser; HTTP's origin-form still
    // requires a leading slash. Fragments are client-side only and therefore
    // must not be sent on the wire.
    let (authority, path) = match rest.find(|c| matches!(c, '/' | '?' | '#')) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], rest[i..].to_string()),
        Some(i) if rest.as_bytes()[i] == b'?' => (&rest[..i], format!("/{}", &rest[i..])),
        Some(i) => (&rest[..i], "/".to_string()),
        None => (rest, "/".to_string()),
    };
    // RFC 3986: authority = [ userinfo "@" ] host [ ":" port ]. Split at the
    // LAST '@' (userinfo may itself contain an encoded/raw '@').
    let (userinfo, hostport) = match authority.rfind('@') {
        Some(i) => (Some(authority[..i].to_string()), &authority[i + 1..]),
        None => (None, authority),
    };
    let (host, port) = match hostport.rfind(':') {
        Some(i) => {
            let (h, p) = (&hostport[..i], &hostport[i + 1..]);
            let pn: u16 = p.parse().map_err(|_| format!("bad port in {url}"))?;
            (h.to_string(), pn)
        }
        None => (hostport.to_string(), if scheme { 443 } else { 80 }),
    };
    Ok((scheme, host, port, path, userinfo))
}

pub fn http_perform_request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    max_redirects: usize,
    tls_config: Option<Arc<ClientConfig>>,
) -> std::io::Result<HttpResponse> {
    http_perform_request_with_timeout(
        method,
        url,
        headers,
        body,
        Duration::from_secs(30),
        max_redirects,
        tls_config,
        &mut http_no_server_trust_gate,
    )
}

/// The server-trust gate of a request whose TLS config does its own chain
/// verification (or of a plain-HTTP request): accepts every chain. See
/// [`http_perform_request_with_timeout`]'s `server_trust`.
pub fn http_no_server_trust_gate(
    _host: &str,
    _port: u16,
    _peer_chain: &[Vec<u8>],
) -> std::io::Result<()> {
    Ok(())
}

/// Performs one logical HTTP request with an end-to-end deadline.  Redirects
/// share the same deadline: `HttpRequest.timeout(Duration)` is a timeout for
/// the request, not a fresh 30-second allowance for every hop.
///
/// `server_trust` (gc-common w14-a) is run by the rustls exchange
/// (`http_exchange_rustls`) on every hop's host, port and peer chain, leaf
/// first, once the handshake has finished and BEFORE the request is written;
/// an `Err` aborts the hop. The host is the one this hop dialled (gc-common
/// w15-a: it is what endpoint identification compares the leaf against), and
/// the port too (gc-common w16-a: the `SSLEngine` handed to an
/// `X509ExtendedTrustManager` reports both). It is how `HttpClient.send` over an `SSLContext` with custom
/// `TrustManager`s consults them: that context's rustls config installs a
/// pass-through verifier, so without the gate nothing checked the chain at
/// all. The native-tls and plain-HTTP exchanges never call it.
pub fn http_perform_request_with_timeout(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: &[u8],
    timeout: Duration,
    max_redirects: usize,
    tls_config: Option<Arc<ClientConfig>>,
    server_trust: &mut dyn FnMut(&str, u16, &[Vec<u8>]) -> std::io::Result<()>,
) -> std::io::Result<HttpResponse> {
    let deadline = Instant::now()
        .checked_add(timeout)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))?;
    let mut current_url = url.to_string();
    let mut current_method = method.to_string();
    let mut current_body = body.to_vec();
    // The chain `HttpResponse.previousResponse()` walks: each redirect hop
    // links to the one before it, exactly the way HotSpot's own client builds
    // it. `None` until the first redirect, and never touched on the common
    // (no-redirect) path.
    let mut prev_resp: Option<Box<HttpResponse>> = None;
    for redirects_followed in 0..=max_redirects {
        let (https, host, port, path, userinfo) = http_parse_url(&current_url)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // JDK parity: a URL carrying user-info (`http://alice:secret@host/…`)
        // sends preemptive `Authorization: Basic base64(userinfo)` unless the
        // caller already staged an explicit Authorization header
        // (sun.net.www.protocol.http.HttpURLConnection does this from
        // url.getUserInfo(); asserted by ResourceTests.useUserInfoToSetBasicAuth).
        let hdrs_with_auth: Vec<(String, String)>;
        let eff_headers: &[(String, String)] = match userinfo {
            Some(ui)
                if !headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization")) =>
            {
                let b64 = String::from_utf8(crate::b64_encode(ui.as_bytes(), 0, false))
                    .unwrap_or_default();
                let mut v = headers.to_vec();
                v.push(("Authorization".to_string(), format!("Basic {b64}")));
                hdrs_with_auth = v;
                &hdrs_with_auth
            }
            _ => headers,
        };
        let mut resp = if https {
            match tls_config.as_ref() {
                Some(config) => http_exchange_rustls(
                    config.clone(),
                    &host,
                    port,
                    &path,
                    &current_method,
                    eff_headers,
                    &current_body,
                    deadline,
                    &mut *server_trust,
                )?,
                None => http_exchange_tls(
                    &host,
                    port,
                    &path,
                    &current_method,
                    eff_headers,
                    &current_body,
                    deadline,
                )?,
            }
        } else {
            http_exchange_plain(
                &host,
                port,
                &path,
                &current_method,
                eff_headers,
                &current_body,
                deadline,
            )?
        };
        // Every hop is stamped with the request that produced it, whether or
        // not it turns out to be a redirect -- `uri()`/`request()` need this
        // for the final response AND for each link `previousResponse()` walks.
        resp.url = current_url.clone();
        resp.method = current_method.clone();
        match resp.status {
            301 | 302 | 303 | 307 | 308 => {
                let loc_opt = resp
                    .headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case("location"))
                    .map(|(_, v)| v.clone());
                if let Some(loc) = loc_opt {
                    // A policy that disallows redirects must expose the first
                    // redirect response, not convert it into a synthetic
                    // "too many redirects" I/O error.
                    if redirects_followed == max_redirects {
                        resp.prev = prev_resp.take();
                        return Ok(resp);
                    }
                    let next = if loc.starts_with("http") {
                        loc
                    } else {
                        let (scheme, h, p, _, _) = http_parse_url(&current_url).map_err(|e| {
                            std::io::Error::new(std::io::ErrorKind::InvalidInput, e)
                        })?;
                        format!(
                            "{}://{}:{}{}",
                            if scheme { "https" } else { "http" },
                            h,
                            p,
                            loc
                        )
                    };
                    current_url = next;
                    if resp.status == 303 {
                        current_method = "GET".into();
                        current_body.clear();
                    }
                    prev_resp = Some(Box::new(resp));
                    continue;
                }
                resp.prev = prev_resp.take();
                return Ok(resp);
            }
            _ => {
                resp.prev = prev_resp.take();
                return Ok(resp);
            }
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "too many redirects",
    ))
}

/// `body` as one HTTP/1.1 chunk plus the last-chunk (RFC 9112 7.1): the
/// framing the JDK's `Http1Request` uses for a body of unknown length. An
/// empty body is the last-chunk alone. (nio20 wave 3, L1.)
pub fn http_chunk_encode(body: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut out = Vec::with_capacity(body.len() + 16);
    if !body.is_empty() {
        let _ = write!(&mut out, "{:x}\r\n", body.len());
        out.extend_from_slice(body);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"0\r\n\r\n");
    out
}

/// Does `req`'s `BodyPublisher` declare an unknown length (`contentLength()
/// < 0`: `ofInputStream`, `fromPublisher(p)`, a user publisher)? The VM's own
/// publisher answers from its declared-length slot without consuming the
/// payload; a JDK-built one is asked (`ofInputStream`'s answer is a constant).
/// `false` for a request with no publisher slot. (nio20 wave 3, L1.)
pub fn re5_body_length_unknown(
    ctx: &mut dyn NativeContext,
    req: ObjectRef,
) -> Result<bool, MethodCallFailed> {
    if ctx.object_num_fields(req) <= RE5_REQUEST_BODY_PUBLISHER {
        return Ok(false);
    }
    let bp = match ctx.get_field(req, RE5_REQUEST_BODY_PUBLISHER) {
        Value::Object(Some(bp)) => bp,
        _ => return Ok(false),
    };
    let own = ctx
        .class_name_of_id(ctx.class_id_of_object(bp))
        .is_some_and(|n| n == "java/net/http/HttpRequest$BodyPublisher");
    if own {
        return Ok(matches!(ctx.get_field(bp, RE5_BP_DECLARED_LENGTH), Value::Long(n) if n < 0));
    }
    match ctx.invoke_virtual(bp, "contentLength", "()J", &[])? {
        Some(Value::Long(n)) => Ok(n < 0),
        _ => Ok(false),
    }
}

pub fn http_build_request(
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    headers: &[(String, String)],
    body: &[u8],
    default_port: u16,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(256 + body.len());
    use std::io::Write as _;
    let _ = write!(&mut out, "{method} {path} HTTP/1.1\r\n");
    if port == default_port {
        let _ = write!(&mut out, "Host: {host}\r\n");
    } else {
        let _ = write!(&mut out, "Host: {host}:{port}\r\n");
    }
    let mut has_content_length = false;
    let mut has_user_agent = false;
    for (k, v) in headers {
        // A `Transfer-Encoding` the caller framed (`re5_do_request`'s chunked
        // body for an unknown-length publisher) is the message's length; a
        // `Content-Length` beside it is a framing error (RFC 9112 6.3).
        if k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("transfer-encoding") {
            has_content_length = true;
        }
        if k.eq_ignore_ascii_case("user-agent") {
            has_user_agent = true;
        }
        let _ = write!(&mut out, "{k}: {v}\r\n");
    }
    if !has_user_agent {
        out.extend_from_slice(b"User-Agent: cratonvm-phaseE/1.0\r\n");
    }
    if !has_content_length && (!body.is_empty() || matches!(method, "POST" | "PUT" | "PATCH")) {
        let _ = write!(&mut out, "Content-Length: {}\r\n", body.len());
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

/// Real HTTP/1.1 response reader: reads the header block, then reads the
/// body according to the framing the headers actually declare
/// (`Content-Length`, chunked, or close-delimited), stopping as soon as the
/// message is complete instead of unconditionally reading until the peer
/// closes the connection.
///
/// A real HTTP/1.1 peer is entitled to keep a connection open after sending
/// a fully-framed response (keep-alive) -- reading until EOF unconditionally
/// hangs forever on such a connection even though the whole response
/// already arrived. See
/// spring-web-flow-outputstreamwriter-close-corruption-FIXED.md
/// root cause #2 (`JdkClientHttpRequestFactoryTests` hang): confirmed via a
/// live `strace` against a real `MockWebServer` that the server sent a
/// complete 38-byte `Content-Length`-framed response and went straight back
/// to `recv()` waiting for the next keep-alive request, while this client
/// kept `recv()`-ing for a close that was never coming.
///
/// `head_response` must be `true` when the request that elicited this
/// response was a HEAD: RFC 9110 §9.3.2 entitles a HEAD response to carry
/// the same framing headers (`Content-Length`/`Transfer-Encoding`) the
/// corresponding GET would have, while sending NO body bytes at all --
/// waiting for the declared `Content-Length` blocks on data the server will
/// never send until the socket's `SO_RCVTIMEO` fires (`IOException:
/// Resource temporarily unavailable (os error 11)`,
/// `JdkClientHttpRequestFactoryTests.gzipCompressionWithHeadRequest`).
pub fn http_read_response<R: Read>(mut r: R, head_response: bool) -> std::io::Result<HttpResponse> {
    let mut all = Vec::with_capacity(8192);
    let mut buf = [0u8; 4096];

    // Phase 1: read until the response headers are fully present.
    let sep = loop {
        if let Some(pos) = all.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        match r.read(&mut buf) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "connection closed before response headers were complete",
                ));
            }
            Ok(n) => all.extend_from_slice(&buf[..n]),
            Err(e) if is_eintr(&e) => continue,
            Err(e) => return Err(e),
        }
    };

    let header_block = &all[..sep];
    let head_str = std::str::from_utf8(header_block)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut lines = head_str.split("\r\n");
    let status_line = lines
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status"))?;
    let mut parts = status_line.splitn(3, ' ');
    let _ = parts.next();
    let code_str = parts.next().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "missing status code")
    })?;
    let status: i32 = code_str
        .parse()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad status code"))?;
    let mut headers: Vec<(String, String)> = Vec::new();
    let mut chunked = false;
    let mut content_length: Option<usize> = None;
    for line in lines {
        if let Some(colon) = line.find(':') {
            let k = line[..colon].trim().to_string();
            let v = line[colon + 1..].trim().to_string();
            if k.eq_ignore_ascii_case("transfer-encoding") && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            }
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().ok();
            }
            headers.push((k, v));
        }
    }

    // Phase 2: read the body per the declared framing, stopping as soon as
    // it's complete.
    let body_start = sep + 4;
    // 1xx/204/304 never carry a body regardless of the framing headers, and
    // neither does any response to a HEAD request.
    let no_body = head_response || matches!(status, 100..=199 | 204 | 304);

    let body = if no_body {
        Vec::new()
    } else if chunked {
        // Mirrors the server-side chunked-body read loop elsewhere in this
        // file (`http_decode_chunked` returns `Ok` only once the terminating
        // zero-size chunk is present): read more whenever it still errors,
        // until it succeeds or the peer closes/errors.
        loop {
            match http_decode_chunked(&all[body_start..]) {
                Ok(b) => break b,
                Err(_) => match r.read(&mut buf) {
                    Ok(0) => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "connection closed before chunked body was complete",
                        ));
                    }
                    Ok(n) => all.extend_from_slice(&buf[..n]),
                    Err(e) if is_eintr(&e) => continue,
                    Err(e) => return Err(e),
                },
            }
        }
    } else if let Some(n) = content_length {
        while all.len() - body_start < n {
            match r.read(&mut buf) {
                // Peer closed early: return whatever body arrived, matching
                // the previous read-until-EOF behavior's leniency for a
                // short response instead of erroring.
                Ok(0) => break,
                Ok(read) => all.extend_from_slice(&buf[..read]),
                Err(e) if is_eintr(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        let body_region = &all[body_start..];
        body_region[..body_region.len().min(n)].to_vec()
    } else {
        // Neither Content-Length nor chunked: the only remaining HTTP/1.1
        // framing is "read until the connection closes" (legacy
        // close-delimited body) -- a compliant server without either header
        // MUST close the connection to signal the end of the body, so
        // waiting for EOF here is still correct and necessary.
        loop {
            match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => all.extend_from_slice(&buf[..n]),
                Err(e) if is_eintr(&e) => continue,
                Err(e) => return Err(e),
            }
        }
        all[body_start..].to_vec()
    };

    Ok(HttpResponse {
        status,
        headers,
        body,
        ..Default::default()
    })
}

pub fn http_decode_chunked(mut data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len());
    loop {
        let nl = data.windows(2).position(|w| w == b"\r\n").ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "bad chunk header")
        })?;
        let size_str = std::str::from_utf8(&data[..nl])
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let size_str = size_str.split(';').next().unwrap_or("0").trim();
        // FIX(net-phase-e #1): parse the hex chunk-length defensively.
        // `usize::from_str_radix` itself errors (not panics) on numeric
        // overflow, but a crafted-but-in-range giant size still drove
        // `n + 2` to overflow (panic in debug builds) and `&data[..n]`
        // to slice out of range (panic). Reject empty/non-hex sizes, and
        // cap the chunk at a sane maximum so the subsequent arithmetic and
        // slicing can never overflow or wrap — on any violation return a
        // protocol error instead of panicking.
        const MAX_CHUNK: usize = 64 * 1024 * 1024; // 64 MiB hard cap per chunk
        if size_str.is_empty() || !size_str.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad chunk size",
            ));
        }
        let n = usize::from_str_radix(size_str, 16)
            .ok()
            .filter(|&n| n <= MAX_CHUNK)
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "chunk size too large")
            })?;
        data = &data[nl + 2..];
        if n == 0 {
            break;
        }
        // `n <= MAX_CHUNK` guarantees `n + 2` cannot overflow `usize`.
        if data.len() < n + 2 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "chunk truncated",
            ));
        }
        out.extend_from_slice(&data[..n]);
        data = &data[n + 2..];
    }
    Ok(out)
}

pub fn re5_dbg() -> bool {
    crate::nbflags().dbg_re5
}

pub fn http_timeout_remaining(deadline: Instant) -> std::io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::TimedOut, "request timed out"))
}

pub fn http_connect_with_deadline(
    host: &str,
    port: u16,
    deadline: Instant,
) -> std::io::Result<TcpStream> {
    let mut last_error = None;
    for address in (host, port).to_socket_addrs()? {
        let remaining = http_timeout_remaining(deadline)?;
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::AddrNotAvailable,
            "HTTP host resolved to no addresses",
        )
    }))
}

pub trait HttpDeadlineSocket: Read {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
}

impl HttpDeadlineSocket for TcpStream {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(timeout)
    }
}

impl HttpDeadlineSocket for native_tls::TlsStream<EintrStream<TcpStream>> {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.get_ref().get_ref().set_read_timeout(timeout)
    }
}

impl HttpDeadlineSocket for StreamOwned<ClientConnection, TcpStream> {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.sock.set_read_timeout(timeout)
    }
}

impl HttpDeadlineSocket for StreamOwned<ClientConnection, GcBlockingSocket<TcpStream>> {
    fn set_http_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.sock.get_ref().set_read_timeout(timeout)
    }
}

/// A socket whose every blocking operation is bracketed by the GC's
/// blocked-region marking, so a stop-the-world pause that starts while this
/// thread is parked in `recv`/`send` does not wait for a safepoint the thread
/// cannot reach until the peer answers.
///
/// **The wrapper is on the SOCKET on purpose.** `rustls` interleaves socket
/// I/O (`read_tls`, `write_tls`, and `complete_io` beneath `write_all` /
/// `flush` / `Read`) with protocol work (`process_new_packets`), and the
/// protocol work is where the Java upcalls happen —
/// `JavaKeyManagerResolver::resolve` calling `chooseClientAlias` to pick the
/// client certificate. A GC-blocked thread must not run bytecode, so the
/// region has to cover the syscalls and stop there. Marking the syscall itself
/// is the only placement where no caller can widen it by accident; the same
/// argument that put `EintrIo` on the socket rather than at each call site.
///
/// See `t27_tls::gc_blocked_syscall` for the hang this fixes.
pub struct GcBlockingSocket<S> {
    pub inner: S,
}

impl<S> GcBlockingSocket<S> {
    /// `pub` since 2026-08-22: `x509_manager::ocsp_http_post` needs the
    /// same treatment. Its OCSP fetch runs from inside certificate validation,
    /// i.e. from inside the very handshake this wrapper exists for, and it was
    /// doing raw blocking `connect`/`write`/`read` with no region at all — the
    /// precise shape `gc_blocked_syscall`'s doc describes, and the last
    /// handshake-reachable path still doing it. DEFENSIVE: no test is known to
    /// fail on it today (see that call site for what was and was not measured).
    pub fn new(inner: S) -> Self {
        Self { inner }
    }

    /// The underlying socket, for the non-blocking calls (`set_read_timeout`
    /// and friends) that must NOT open a region.
    pub fn get_ref(&self) -> &S {
        &self.inner
    }
}

impl<S: Read> Read for GcBlockingSocket<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let _blocked = crate::t27_tls::gc_blocked_syscall();
        self.inner.read(buffer)
    }
}

impl<S: Write> Write for GcBlockingSocket<S> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let _blocked = crate::t27_tls::gc_blocked_syscall();
        self.inner.write(buffer)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _blocked = crate::t27_tls::gc_blocked_syscall();
        self.inner.flush()
    }
}

/// Re-arms the operating-system receive timeout before every response read so
/// a peer that drips bytes cannot extend a request deadline indefinitely.
pub struct HttpDeadlineReader<S> {
    pub stream: S,
    pub deadline: Instant,
}

impl<S: HttpDeadlineSocket> Read for HttpDeadlineReader<S> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stream
            .set_http_read_timeout(Some(http_timeout_remaining(self.deadline)?))?;
        // EINTR is deliberately NOT absorbed here. It is passed up to
        // `http_read_response`, whose loop calls back into this method, which
        // re-arms `SO_RCVTIMEO` from the deadline above. Retrying in place
        // would skip that re-arm, and Linux restarts the receive timer after
        // every interrupted `recv` — so a signal storm could stretch the
        // caller's deadline without bound.
        match self.stream.read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) =>
            {
                Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "request timed out",
                ))
            }
            other => other,
        }
    }
}

pub fn http_exchange_plain(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    deadline: Instant,
) -> std::io::Result<HttpResponse> {
    let dbg = re5_dbg();
    let t0 = if dbg { Some(Instant::now()) } else { None };
    if dbg {
        eprintln!("[RE5-DBG] {method} {host}:{port}{path} connecting...");
    }
    let connect_result = http_connect_with_deadline(host, port, deadline);
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} connect -> {:?} ({:?} elapsed)",
            connect_result.as_ref().map(|_| "OK").map_err(|e| e.kind()),
            t0.map(|t| t.elapsed())
        );
    }
    let mut stream = connect_result?;
    stream.set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
    let req = http_build_request(method, host, port, path, headers, body, 80);
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} writing {} bytes (body {} bytes): {:?}",
            req.len(),
            body.len(),
            String::from_utf8_lossy(&req[..req.len().min(200)])
        );
    }
    let write_result = stream.write_all(&req);
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} write_all -> {:?} ({:?} elapsed)",
            write_result.as_ref().map(|_| "OK").map_err(|e| e.kind()),
            t0.map(|t| t.elapsed())
        );
    }
    write_result?;
    stream.flush()?;
    if dbg {
        eprintln!("[RE5-DBG] {method} {host}:{port}{path} flushed, reading response...");
    }
    let read_result = http_read_response(
        HttpDeadlineReader { stream, deadline },
        method.eq_ignore_ascii_case("HEAD"),
    );
    if dbg {
        eprintln!(
            "[RE5-DBG] {method} {host}:{port}{path} read_response -> status={:?} err={:?} ({:?} elapsed)",
            read_result.as_ref().ok().map(|r| r.status),
            read_result.as_ref().err().map(|e| e.kind()),
            t0.map(|t| t.elapsed())
        );
    }
    read_result
}

pub fn http_exchange_tls(
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    deadline: Instant,
) -> std::io::Result<HttpResponse> {
    let connector = native_tls::TlsConnector::builder()
        .build()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS init: {e}")))?;
    let tcp = http_connect_with_deadline(host, port, deadline)?;
    tcp.set_read_timeout(Some(http_timeout_remaining(deadline)?))?;
    tcp.set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
    // `native_tls` runs the handshake inside `connect`, so the only place an
    // EINTR can be absorbed is underneath the socket it is handed. Same
    // rationale as the rustls sibling below.
    let mut tls = connector
        .connect(host, EintrStream::new(tcp))
        .map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::Other, format!("TLS handshake: {e}"))
        })?;
    let req = http_build_request(method, host, port, path, headers, body, 443);
    tls.write_all(&req)?;
    retry_eintr(|| tls.flush())?;
    http_read_response(
        HttpDeadlineReader {
            stream: tls,
            deadline,
        },
        method.eq_ignore_ascii_case("HEAD"),
    )
}

/// HTTPS exchange using the rustls configuration scoped to an explicit Java
/// SSLContext. Unlike native-tls, this honours the context's trust/key manager
/// state, which is required by `HttpClient.Builder.sslContext(...)`.
///
/// `server_trust` is handed `host` and the peer chain after the handshake and
/// before the request is written; see [`http_perform_request_with_timeout`].
pub fn http_exchange_rustls(
    config: Arc<ClientConfig>,
    host: &str,
    port: u16,
    path: &str,
    method: &str,
    headers: &[(String, String)],
    body: &[u8],
    deadline: Instant,
    server_trust: &mut dyn FnMut(&str, u16, &[Vec<u8>]) -> std::io::Result<()>,
) -> std::io::Result<HttpResponse> {
    // `connect` blocks for up to the caller's whole remaining deadline, and
    // this path — unlike the plain-HTTP one — has no blocking region around
    // the exchange (see `t27_tls::gc_blocked_syscall`), so it gets its own.
    let tcp = {
        let _blocked = crate::t27_tls::gc_blocked_syscall();
        http_connect_with_deadline(host, port, deadline)?
    };
    tcp.set_read_timeout(Some(http_timeout_remaining(deadline)?))?;
    tcp.set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
    let server_name = ServerName::try_from(host.to_owned()).map_err(|e| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("bad TLS server name: {e}"),
        )
    })?;
    let conn = ClientConnection::new(config, server_name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, format!("TLS init: {e}")))?;
    // `GcBlockingSocket` marks this thread GC-blocked around each syscall and
    // only around each syscall, so `process_new_packets` below — which is
    // where `JavaKeyManagerResolver` runs Java to choose the client
    // certificate — executes as a normal mutator. See its doc comment.
    let mut tls: StreamOwned<ClientConnection, GcBlockingSocket<TcpStream>> =
        StreamOwned::new(conn, GcBlockingSocket::new(tcp));
    // Every socket op below goes through `EintrIo`. A blocking `recv` on a
    // socket that carries `SO_RCVTIMEO` — which the two `set_*_timeout` calls
    // in this loop install on purpose, to keep the caller's deadline honest —
    // is NOT restarted by `SA_RESTART`, so CratonVM's own cross-thread JIT
    // root-scan `SIGUSR2` surfaced here as
    // `IOException: TLS handshake read: Interrupted system call (os error 4)`.
    // See `cratonvm_native_io::eintr` and
    // `jdk-httpclient-sslbundle-tls-handshake-eintr-FIXED-20260806.md`.
    while tls.conn.is_handshaking() {
        if tls.conn.wants_write() {
            tls.sock
                .get_ref()
                .set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
            tls.conn
                .write_tls(&mut EintrIo::new(&mut tls.sock))
                .map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("TLS handshake write: {e}"),
                    )
                })?;
        }
        if tls.conn.wants_read() {
            tls.sock
                .get_ref()
                .set_read_timeout(Some(http_timeout_remaining(deadline)?))?;
            let count = tls
                .conn
                .read_tls(&mut EintrIo::new(&mut tls.sock))
                .map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::Other,
                        format!("TLS handshake read: {e}"),
                    )
                })?;
            if count == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "TLS handshake: peer closed connection",
                ));
            }
            tls.conn.process_new_packets().map_err(|e| {
                std::io::Error::new(std::io::ErrorKind::Other, format!("TLS handshake: {e}"))
            })?;
        }
    }
    // Captured HERE, once the handshake loop above has exited (i.e. the
    // connection has actually negotiated something) and BEFORE `tls` is moved
    // into `http_read_response` below: `ConnectionCommon`'s introspection
    // methods borrow `tls.conn`, and the read call needs to own the whole
    // `StreamOwned`. Mirrors exactly what `t27_tls::build_synthetic_ssl_session`
    // reads off an `SSLEngine`'s own `rustls::ClientConnection` -- protocol,
    // negotiated cipher suite (translated to its JSSE name) and the peer's DER
    // certificate chain -- so `HttpResponse.sslSession()` (added below, see
    // docs/feature-designs/native-dispatch-architecture.md)
    // reports what this handshake actually negotiated instead of a stub.
    let tls_info = {
        let protocol = match tls.conn.protocol_version() {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
            _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
        }
        .to_string();
        let cipher_suite = tls
            .conn
            .negotiated_cipher_suite()
            .map(|cs| crate::t27_tls::suite_to_java_cipher_name_pub(cs.suite()))
            .unwrap_or_else(|| {
                crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string()
            });
        let peer_certs_der: Vec<Vec<u8>> = tls
            .conn
            .peer_certificates()
            .map(|certs| certs.iter().map(|c| c.as_ref().to_vec()).collect())
            .unwrap_or_default();
        Re5TlsSessionInfo {
            protocol,
            cipher_suite,
            peer_certs_der,
        }
    };
    // gc-common w14-a (common-w13a-httpclient-custom-trustmanager-is-never-
    // consulted): the caller's trust decision, at real JSSE's ordering —
    // after the handshake, before a single request byte (headers, body,
    // credentials) reaches a peer nobody has vouched for. A config built over
    // an `SSLContext` with custom `TrustManager`s verifies nothing itself.
    server_trust(host, port, &tls_info.peer_certs_der)?;
    let request = http_build_request(method, host, port, path, headers, body, 443);
    tls.sock
        .get_ref()
        .set_write_timeout(Some(http_timeout_remaining(deadline)?))?;
    // `write_all` is left bare on purpose: `std`'s default impl already
    // reissues on `Interrupted` AND advances past the bytes it did place, which
    // a `retry_eintr` wrapper around the whole call could not do — it would
    // restart from offset 0. `flush` has no partial state, so wrapping it is
    // safe, and it needs the wrapper: `rustls`' `Stream::flush` drives
    // `complete_io` internally, where the socket's EINTR reaches us from a
    // method no `EintrIo` above is wrapping.
    tls.write_all(&request)?;
    retry_eintr(|| tls.flush())?;
    let mut resp = http_read_response(
        HttpDeadlineReader {
            stream: tls,
            deadline,
        },
        method.eq_ignore_ascii_case("HEAD"),
    )?;
    resp.tls = Some(tls_info);
    Ok(resp)
}

pub fn huc_extract_req_headers(ctx: &dyn NativeContext, hdrs: Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    if let Value::Object(Some(a)) = hdrs {
        let len = ctx.array_length(a);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(a, i) {
                let line = ctx.read_string(s).unwrap_or_default();
                if let Some(colon) = line.find(':') {
                    let k = line[..colon].trim().to_string();
                    let v = line[colon + 1..].trim().to_string();
                    if !k.is_empty() {
                        out.push((k, v));
                    }
                }
            }
        }
    }
    out
}

/// The connection's URL text.
///
/// gc-common w20-d: `this` is `&mut` and CURRENT on return. A real-JDK URL's
/// field 0 is only its protocol, so this falls through to the URL's
/// `toString()` (Java) on every real carrier; every caller then used its
/// connection through the address from before that call (`huc_perform`
/// pinned the stale one).
pub fn huc_url_string(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> String {
    let url = match ctx.get_field(*this, HUC_URL) {
        Value::Object(Some(u)) => u,
        _ => return String::new(),
    };
    if let Some(txt) = read_field_string(ctx, url, 0) {
        if txt.contains("://") {
            return txt;
        }
    }
    let this_pin = ctx.pin_native_root(*this);
    let res = ctx.invoke_virtual(url, "toString", "()Ljava/lang/String;", &[]);
    let text = match res {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => String::new(),
    };
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    text
}

/// Return the originating URL's external form for one of our synthetic
/// HttpURLConnection carriers. Unlike `huc_url_string`, this deliberately uses
/// `toExternalForm`: a real-JDK URL's field zero is only its protocol (for
/// example, `file`), not a complete URL suitable for filesystem metadata.
/// Scan the parsed response-header lines (`"Name: value"` strings, stored
/// under `HUC_RESP_HEADERS`) for `key` (case-insensitive) and return its
/// value. Shared by `getHeaderField`/`getLastModified`/`getHeaderFieldDate`
/// so all three agree on the same real HTTP(S) response headers.
pub fn huc_find_header_value(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key: &str,
) -> Option<String> {
    if let Value::Object(Some(arr)) = ctx.get_field(this, HUC_RESP_HEADERS) {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                let line = ctx.read_string(s).unwrap_or_default();
                if let Some(colon) = line.find(':') {
                    if line[..colon].trim().eq_ignore_ascii_case(key) {
                        return Some(line[colon + 1..].trim().to_string());                    }                }            }        }    }
    None
}

/// Parse an HTTP-date into epoch milliseconds (UTC). Real `HttpURLConnection
/// .getHeaderFieldDate` accepts all three formats RFC 9110 (`https://www.rfc-
/// editor.org/rfc/rfc9110#section-5.6.7`) permits for received messages; only
/// RFC 1123 (`"EEE, dd MMM yyyy HH:mm:ss 'GMT'"`, e.g. `"Wed, 09 Apr 2014
/// 09:57:42 GMT"`) is implemented here since it's what every real server
/// (and this codebase's own outgoing `Date`/`Last-Modified` formatting)
/// emits -- the two legacy formats (RFC 850, asctime) are vanishingly rare
/// in practice. Returns `None` on anything else so callers fall back to
/// their caller-supplied default, matching the JDK contract for an
/// unparseable date header.
pub fn parse_rfc1123_date_millis(s: &str) -> Option<i64> {
    let s = s.trim();
    // "Wed, 09 Apr 2014 09:57:42 GMT"
    let rest = s.split_once(", ").map(|(_, r)| r).unwrap_or(s);
    let mut parts = rest.split_whitespace();
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: i64 = parts.next()?.parse().ok()?;
    let time = parts.next()?;
    let mut hms = time.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let min: i64 = hms.next()?.parse().ok()?;
    let sec: i64 = hms.next()?.parse().ok()?;
    let days = days_from_civil_utc(year, month, day);
    let secs_of_day = hour * 3600 + min * 60 + sec;
    Some((days * 86_400 + secs_of_day) * 1_000)
}

/// Days since the Unix epoch (1970-01-01) for a proleptic-Gregorian civil
/// date, UTC. Howard Hinnant's `days_from_civil` algorithm
/// (`https://howardhinnant.github.io/date_algorithms.html#days_from_civil`)
/// -- avoids pulling in a date/time crate for this one conversion.
pub fn days_from_civil_utc(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// gc-common w20-d: `this` is `&mut` and CURRENT on return, as for
/// [`huc_url_string`]: `toExternalForm` is Java.
pub fn huc_origin_url_string(ctx: &mut dyn NativeContext, this: &mut ObjectRef) -> String {
    let url = match ctx.get_field(*this, HUC_URL) {
        Value::Object(Some(u)) => u,
        _ => return String::new(),
    };
    let this_pin = ctx.pin_native_root(*this);
    let res = ctx.invoke_virtual(url, "toExternalForm", "()Ljava/lang/String;", &[]);
    *this = ctx.read_native_pin(this_pin, *this);
    ctx.unpin_native_roots(this_pin);
    match res {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => huc_url_string(ctx, this),
    }
}

/// Run the connection's request once and cache the response on it.
///
/// gc-common w20-d: `this` is `&mut` and CURRENT on return. The response's
/// Strings and arrays are allocated here (and `huc_url_string` can run Java);
/// every caller read the response slots back through its pre-call address.
pub fn huc_perform(ctx: &mut dyn NativeContext, this_io: &mut ObjectRef) -> MethodCallResult {
    let mut this = *this_io;
    if ctx.get_field(this, HUC_CONNECTED).as_int().unwrap_or(0) != 0 {
        return Ok(None);
    }
    let method = read_field_string_or(ctx, this, HUC_METHOD, "GET");
    let hdrs_val = ctx.get_field(this, HUC_REQ_HEADERS);
    let headers = huc_extract_req_headers(ctx, hdrs_val);
    let url = huc_url_string(ctx, &mut this);
    *this_io = this;
    if url.is_empty() {
        return Err(ioex("HttpURLConnection: missing URL"));
    }
    // gc-common w20-d: the request (connect, write, read; 10 s timeouts) ran
    // as an ordinary native call holding no heap reference it needed, so a
    // stop-the-world collection requested by another thread waited for this
    // one until the peer answered. It runs in a GC-blocking region now (the
    // `http2.rs::blocking_http_exchange` shape), with `this` pinned across
    // it: the pin used to be taken only after the request.
    let this_pin = ctx.pin_native_root(this);
    ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
    let resp = http_perform_request(&method, &url, &headers, &[], 10, None);
    ctx.end_blocking_region();
    this = ctx.read_native_pin(this_pin, this);
    *this_io = this;
    let resp = match resp {
        Ok(resp) => resp,
        Err(e) => {
            ctx.unpin_native_roots(this_pin);
            return Err(ioex(format!("HTTP {method} {url}: {e}")));
        }
    };
    // GC-SAFETY: everything from here on allocates on the Java heap (the
    // headers array, one `String` per response header, the body byte[]) --
    // `this` is a bare `ObjectRef` that a moving GC triggered by any of
    // those allocations can relocate out from under us. Without pinning,
    // the LAST `set_field(this, ...)` calls below silently wrote through a
    // stale `this`, so `getLastModified()`/`getHeaderField(name)` (called
    // right after `huc_perform` returns) read back an empty/absent
    // `HUC_RESP_HEADERS` even though the real HTTP response genuinely
    // contained e.g. a `Last-Modified` header -- confirmed by tracing the
    // parsed response (correct) against the object's field afterward
    // (missing). Mirrors the established pin/re-read pattern used
    // throughout `native-collections` for the identical hazard.
    ctx.set_field(this, HUC_CODE, Value::Int(resp.status));
    let mut hdr_arr = ctx.new_ref_array(ClassId::new(0), resp.headers.len());
    let hdr_pin = ctx.pin_native_root(hdr_arr);
    for (i, (k, v)) in resp.headers.iter().enumerate() {
        let s = ctx.create_string(&format!("{k}: {v}"));
        this = ctx.read_native_pin(this_pin, this);
        hdr_arr = ctx.read_native_pin(hdr_pin, hdr_arr);
        ctx.set_array_element(hdr_arr, i, Value::Object(Some(s)));
    }
    this = ctx.read_native_pin(this_pin, this);
    hdr_arr = ctx.read_native_pin(hdr_pin, hdr_arr);
    ctx.set_field(this, HUC_RESP_HEADERS, Value::Object(Some(hdr_arr)));
    let body_arr = new_java_byte_array(ctx, &resp.body);
    this = ctx.read_native_pin(this_pin, this);
    ctx.set_field(this, HUC_BODY, Value::Object(Some(body_arr)));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
    ctx.unpin_native_roots(this_pin);
    *this_io = this;
    Ok(None)
}

/// Resolve a `classpath:` resource through the current thread's context
/// classloader, mirroring Tomcat's real `ClasspathURLStreamHandler.openConnection`
/// (`Thread.currentThread().getContextClassLoader().getResourceAsStream(path)`).
///
/// `find_resource` only sees the static bootstrap/ext/app classpath; it cannot
/// reach a deployed webapp's `WEB-INF/classes`. The webapp's
/// `WebappClassLoaderBase` overrides `getResourceAsStream` and resolves those via
/// its `WebResourceRoot` (real Tomcat bytecode), so going through the live TCCL
/// picks them up. Returns the loader's own `InputStream` (already a real object),
/// or `None` when there is no context loader or it has no such resource — the
/// caller then falls back to `FileNotFoundException`, matching the real handler.
///
/// Ordering note: the loader ref is only held across a single small
/// `create_string` allocation before its `getResourceAsStream` call, keeping the
/// moving-GC exposure minimal (same shape as the `toExternalForm` hop above).
pub fn classpath_resource_via_context_loader(
    ctx: &mut dyn NativeContext,
    name: &str,
) -> Option<ObjectRef> {
    let thread = match ctx.invoke(
        "java/lang/Thread",
        "currentThread",
        "()Ljava/lang/Thread;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(t)))) => t,
        _ => return None,
    };
    let loader = match ctx.invoke_virtual(
        thread,
        "getContextClassLoader",
        "()Ljava/lang/ClassLoader;",
        &[],
    ) {
        Ok(Some(Value::Object(Some(l)))) => l,
        _ => return None,
    };
    let name_s = ctx.create_string(name);
    match ctx.invoke_virtual(
        loader,
        "getResourceAsStream",
        "(Ljava/lang/String;)Ljava/io/InputStream;",
        &[Value::Object(Some(name_s))],
    ) {
        Ok(Some(Value::Object(Some(stream)))) => Some(stream),
        _ => None,
    }
}

/// True when `s` is a usable *full URL* (carries a scheme), as opposed to a bare
/// `host:port` authority. A real `java.net.URL`'s field index 5 is the
/// `authority` (e.g. `"localhost:8080"`), NOT a synthetic full-URL cache — so a
/// plain `contains(':')` test wrongly accepts the authority as the URL, making
/// `toExternalForm()`/`openStream` see scheme `"localhost:8080"`
/// (`unsupported scheme: localhost:<port>`). Discriminator: in a real URL the
/// text after the FIRST ':' is `"//..."` or a path/opaque part; in an authority
/// it is the numeric port. So reject when everything after the first ':' is
/// ASCII digits (a port).
///
/// A real URL's authority may ALSO carry user-info (`alice:secret@localhost:8080`),
/// where the text after the first ':' is NOT all digits — the digits-only test
/// alone then misreads the authority as a full URL with scheme `alice`
/// (`URL.openStream: unsupported scheme: alice:secret@localhost:<port>`,
/// ResourceTests.useUserInfoToSetBasicAuth). Discriminator: an authority's
/// user-info '@' appears before any '/', while a full URL's first '/' comes
/// immediately after `scheme:` (e.g. `http://u:p@h/x` → `http:` precedes the
/// first '/', no '@' in it). So additionally reject when the segment before
/// the first '/' contains '@'.
pub fn field5_is_full_url(s: &str) -> bool {
    let before_slash = s.split('/').next().unwrap_or(s);
    if s.starts_with('[') {
        return false;
    }
    if before_slash.contains('@') {
        return false;
    }
    match s.split_once(':') {
        Some((scheme, rest)) => {
            // `rest.parse::<i64>()`, not "every byte is a digit": `h:-1` is a
            // PORT too — HotSpot answers `getPort() == -1` and keeps `h:-1` as
            // the authority — and the digits-only test read it as a scheme, so
            // `new URL("http://h:-1/p").toExternalForm()` handed back the bare
            // authority `h:-1` in place of the whole URL.
            !scheme.is_empty() && !rest.is_empty() && rest.parse::<i64>().is_err()
        }
        None => false,
    }
}

/// Slot indices of CratonVM's synthetic `java.net.URL` layout, mirrored from
/// `net_uri_inet::url_parse` (which is the writer). Slots 0..=2 coincide with
/// the real-JDK `URL` field order (`protocol`, `host`, `port`), slot 3 is the
/// path (read by `lib.rs`'s `url_path_or_file_field`), slot 4 the query and
/// slot 5 the cached full external form.
pub const URLS_PROTOCOL: usize = 0;
pub const URLS_HOST: usize = 1;
pub const URLS_PORT: usize = 2;
pub const URLS_QUERY: usize = 4;
pub const URLS_FULL: usize = 5;

/// `true` when `s` is a bare URL scheme token (`https`, `jar`, `file`, …) as
/// opposed to a whole URL or an authority. Used to tell CratonVM's two
/// synthetic URL layouts apart: `url_parse` puts the SCHEME in slot 0, while
/// the older classloader helpers put the FULL URL string there.
pub fn url_is_scheme_token(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
}

/// Recover a synthetic URL's full external form from its raw slots.
///
/// A synthetic `java/net/URL` is minted by `ensure_synthetic_class`, which
/// declares a slot COUNT and ZERO field names — so `get_field_by_name` on such
/// an object always answers `Object(None)` and every by-name getter silently
/// degrades to its default. Both synthetic layouts cache the full URL at slot
/// 5; the legacy classloader layout also puts it at slot 0.
pub fn url_raw_full_string(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<String> {
    let n = ctx.object_num_fields(this);
    for slot in [URLS_FULL, URLS_PROTOCOL] {
        if n <= slot {
            continue;
        }
        if let Value::Object(Some(s)) = ctx.get_field(this, slot) {
            if let Some(text) = ctx.read_string(s) {
                if field5_is_full_url(&text) {
                    return Some(text);                }            }        }    }
    None
}

/// Split a full URL string the way `java.net.URL` does:
/// `protocol:[//[userinfo@]host[:port]]path[?query][#ref]`.
///
/// Returns `(protocol, host, port, path, query, ref)` with `port = -1` when no
/// port is present and `None` for an absent (as opposed to empty) query/ref.
#[allow(clippy::type_complexity)]
pub fn url_components(spec: &str) -> (String, String, i32, String, Option<String>, Option<String>) {
    let (scheme, rest) = match spec.find(':') {
        Some(i) if url_is_scheme_token(&spec[..i]) => (spec[..i].to_string(), &spec[i + 1..]),
        _ => (String::new(), spec),
    };
    let (rest, reff) = match rest.find('#') {
        Some(i) => (&rest[..i], Some(rest[i + 1..].to_string())),
        None => (rest, None),
    };
    let (authority, path_query) = match rest.strip_prefix("//") {
        Some(after) => match after.find(['/', '?']) {
            Some(i) => (&after[..i], &after[i..]),
            None => (after, ""),
        },
        None => ("", rest),
    };
    // Strip any `userinfo@` prefix — it is not part of the host.
    let host_port = match authority.rfind('@') {
        Some(i) => &authority[i + 1..],
        None => authority,
    };
    let (host, port) = if let Some(close) = host_port.strip_prefix('[').and_then(|_| {
        // IPv6 literal: `[::1]:8080` — the port colon is the one AFTER `]`.
        host_port.find(']')
    }) {
        let port = host_port[close + 1..]
            .strip_prefix(':')
            .and_then(|p| p.parse::<i32>().ok())
            .unwrap_or(-1);
        (host_port[..=close].to_string(), port)
    } else {
        match host_port.rfind(':') {
            Some(i) => match host_port[i + 1..].parse::<i32>() {
                Ok(p) => (host_port[..i].to_string(), p),
                // Not a port (e.g. a malformed authority) — keep it all as host.
                Err(_) => (host_port.to_string(), -1),
            },
            None => (host_port.to_string(), -1),
        }
    };
    let (path, query) = match path_query.find('?') {
        Some(i) => (
            path_query[..i].to_string(),
            Some(path_query[i + 1..].to_string()),
        ),
        None => (path_query.to_string(), None),
    };
    (
        scheme,
        host,
        port,
        path,
        query.filter(|q| !q.is_empty()),
        reff.filter(|f| !f.is_empty()),
    )
}

/// Read a URL component reference the layout-neutral way: by field NAME first
/// (a real `java.net.URL`, or one of our 13-slot real-shaped synthetics), then
/// by raw SLOT (the synthetic layouts, whose fields have no names at all).
/// Returns the EXISTING `String` reference on a hit — callers must not
/// re-create it, both to keep object identity and to keep these hot getters
/// allocation-free. `None` means "fall back to re-parsing the full URL".
pub fn url_component_ref(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    slot: usize,
) -> Option<ObjectRef> {
    if let Value::Object(Some(s)) = ctx.get_field_by_name(this, name) {
        return Some(s);
    }
    if ctx.object_num_fields(this) > slot {
        if let Value::Object(Some(s)) = ctx.get_field(this, slot) {
            return Some(s);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// The `https:` carrier's TLS session
// ---------------------------------------------------------------------------
//
// WHY THIS EXISTS. `URL.openConnection()` on an `https:` URL used to hand back
// `javax/net/ssl/HttpsURLConnection` ITSELF, which is `abstract`. On HotSpot 25:
//
//     $ javap -p --module java.base javax.net.ssl.HttpsURLConnection
//       public abstract java.lang.String getCipherSuite();
//       public abstract java.security.cert.Certificate[] getLocalCertificates();
//       public abstract java.security.cert.Certificate[] getServerCertificates()
//
// so all three threw `AbstractMethodError: ... has no Code attribute` on a
// connection that had completed a real, certificate-verified handshake, and the
// CONCRETE `getSSLSession()` on the same class answered `Optional.empty()`
// SILENTLY -- "this connection has no TLS session" on a connection that has
// one, with no diagnostic for a caller that branches on it. Measured against
// HotSpot 25 in `docs/feature-designs/native-dispatch-architecture.md` section 3
// and re-measured for this record.
//
// The carrier is now the concrete `HttpsURLConnectionImpl`, as HotSpot returns.
// That alone is NOT a fix: every method on that class is
// `getfield delegate; invokevirtual DelegateHttpsURLConnection....`, and this
// VM's carrier is allocated rather than constructed, so `delegate` is null and
// the swap by itself only trades the `AbstractMethodError` for an NPE. The six
// accessors below are the other half; they are registered on BOTH the Impl and
// the abstract base so neither receiver can reach that bytecode.
//
// THE UNCONNECTED SHAPE IS MEASURED, NOT ASSUMED. On HotSpot 25, all six
// accessors on a connection that has not completed a handshake throw the SAME
// exception:
//
//     class = sun.net.www.protocol.https.HttpsURLConnectionImpl
//     getCipherSuite        THREW java.lang.IllegalStateException: connection not yet open
//     getServerCertificates THREW java.lang.IllegalStateException: connection not yet open
//     getLocalCertificates  THREW java.lang.IllegalStateException: connection not yet open
//     getPeerPrincipal      THREW java.lang.IllegalStateException: connection not yet open
//     getLocalPrincipal     THREW java.lang.IllegalStateException: connection not yet open
//     getSSLSession         THREW java.lang.IllegalStateException: connection not yet open
//
// `getSSLSession()` included -- so today's silent `Optional.empty()` is wrong
// even for the unconnected case, and answering `IllegalStateException` here
// cannot regress a caller relative to HotSpot.
//
// GC. The table holds PLAIN DATA only (two `String`s and the peer chain's DER
// bytes), never an `ObjectRef`, so it needs no rooting in any collector path --
// the same rule `sock_side_table` above states and for the same reason. It is
// keyed by `NativeObjKey` (VM identity + the carrier's weak lock key since
// gc-common w28-a; its `identityHashCode` before), which is stable across
// relocation and scoped to one VM, not by a raw address.

/// One completed client handshake, as the six `HttpsURLConnection` session
/// accessors need to answer it.
#[derive(Clone, Debug, Default)]
pub struct HttpsCarrierSession {
    /// `TLSv1.3` / `TLSv1.2`, exactly as the handshake reported it.
    pub protocol: String,
    /// The negotiated cipher suite name, e.g. `TLS_AES_256_GCM_SHA384`.
    pub cipher: String,
    /// The peer's certificate chain, leaf first, DER-encoded.
    pub peer_chain_der: Vec<Vec<u8>>,
    /// Global-root HANDLE (never an `ObjectRef`) for the ONE `SSLSession`
    /// object this carrier hands out; `0` until the first accessor mints it.
    ///
    /// A handle is plain data, so the "this table holds no `ObjectRef`" rule
    /// stated above still holds exactly: the collector owns the reference, and
    /// [`NativeContext::resolve_global_root`] hands back its current, possibly
    /// relocated address. This is the same shape `jca::provider_chain` and
    /// `jboss_jdkspecific` already use for a cached Java object.
    ///
    /// WHY IT HAS TO BE CACHED. MEASURED, `RSslLiveSession` on
    /// `9964ca733`, 2026-08-17:
    ///
    /// ```text
    /// CK RSslLiveSession client.sslSession.sameObjectTwice = false  WANT true
    /// ```
    ///
    /// HotSpot's `HttpsURLConnection` holds one `SSLSession` for the
    /// connection's lifetime; every accessor answers from it and
    /// `getSSLSession()` wraps *that* object in a fresh `Optional`. This VM
    /// minted a new session on every accessor call, which was invisible while
    /// `getId()` answered `byte[0]` and became visible the moment the ids were
    /// real — `getId()` is seeded from the object's identity, so two reads of
    /// one connection disagreed about the same handshake.
    pub session_root: usize,
    /// The endpoint this connection DIALLED, as the URL named it — never
    /// derived from the peer certificate and never from SNI.
    ///
    /// G51-1 §1 MEASURED both derivations wrong, with the IP-literal request
    /// as the counter-example: leaf subject `CN=localhost`, SNI `localhost`,
    /// and HotSpot's `getPeerHost()` answering `127.0.0.1`. The port is the
    /// resolved one, so a default-port `https:` URL records `443` — which is
    /// what the connection actually connected to.
    ///
    /// Carried here rather than looked up later because
    /// [`https_session_object`] mints at the first ACCESSOR call, arbitrarily
    /// long after the request returned; G51-1 §2 refused a "last dialled
    /// endpoint" latch for exactly that reason, since `RSslLiveSession`'s
    /// `distinct` family opens a second connection before re-reading the
    /// first session and a latch would answer one connection's endpoint for
    /// another's session.
    pub peer_host: String,
    pub peer_port: i32,
    /// The carrier's address when its handshake was recorded (gc-common
    /// w17-a), its weak owner; `0` records none. [`gc_sweep_net_socket_rows`]
    /// re-addresses it after a move and drops the row, releasing
    /// `session_root`, once the carrier is dead. Without it, a carrier that
    /// was never drained, closed or disconnected kept its row, its peer chain
    /// and a global root on its `SSLSession` until the VM exited, and a later
    /// carrier handed the dead one's identity hash answered the dead one's
    /// handshake instead of `connection not yet open`.
    pub owner: usize,
}

pub fn https_carrier_sessions() -> &'static OrderedPlMutex<HashMap<NativeObjKey, HttpsCarrierSession>> {
    static T: OnceLock<OrderedPlMutex<HashMap<NativeObjKey, HttpsCarrierSession>>> =
        OnceLock::new();
    T.get_or_init(|| OrderedPlMutex::new(HashMap::new(), LockLevel::Scratch))
}

/// Record the TLS session a completed `https:` request negotiated, against the
/// `HttpsURLConnection` carrier that made it.
///
/// Called from the handshake path in `http_url_connection.rs`, which is the one
/// place that has the protocol, the cipher suite, the peer chain and the
/// dialled endpoint in hand at the same time. Without a call to this, every accessor below answers
/// `IllegalStateException: connection not yet open`, which is HotSpot's own
/// answer for a connection that has not handshaken -- so a missing call is a
/// missing ANSWER, never a wrong one.
///
/// THE CALL SITE HAS ARRIVED. It is a single line inside
/// `http_url_connection.rs`'s `huc_verify_hostname` (STEP 0), which is the only
/// place holding all three values at once, and that file's own test asserts the
/// line stays ABOVE the built-in check's early return. The note that used to
/// stand here said this function was uncalled and carried `#[allow(dead_code)]`
/// as "the reviewer's cue that the call site arrived"; the cue has fired.
///
/// A second handshake recorded against the same carrier REPLACES the entry, so
/// the previously cached session object is released here rather than left
/// rooted for the life of the VM — the `provider_chain::set_cached_provider`
/// idiom. That is why this takes `&mut dyn NativeContext`: releasing a global
/// root is a mutation. The existing call site passes a `&mut dyn
/// NativeContext` already, so the signature change is invisible to it.
pub fn record_https_carrier_session(
    ctx: &mut dyn NativeContext,
    connection: ObjectRef,
    protocol: &str,
    cipher: &str,
    peer_chain_der: &[Vec<u8>],
    peer_host: &str,
    peer_port: u16,
) {
    // `connection` is the carrier's CURRENT address (the caller re-reads it
    // through its pin), which makes it a valid weak owner for the sweep and a
    // valid address to mint its key at.
    let key = mint_native_obj_key(&*ctx, connection);
    let stale = {
        let mut table = https_carrier_sessions().lock();
        let stale = table.get(&key).map(|e| e.session_root).unwrap_or(0);
        table.insert(
            key,
            HttpsCarrierSession {
                protocol: protocol.to_string(),
                cipher: cipher.to_string(),
                peer_chain_der: peer_chain_der.to_vec(),
                session_root: 0,
                peer_host: peer_host.to_string(),
                peer_port: i32::from(peer_port),
                owner: connection.as_ptr() as usize,
            },
        );
        stale
    };
    if stale != 0 {
        ctx.remove_global_root(stale);
    }
    // Allocates nothing: the session roots of carriers the sweep found dead.
    net_release_deferred(ctx);
}

/// Forget the recorded handshake for a carrier that has been RECYCLED, and
/// release the `SSLSession` object it was holding.
///
/// MEASURED on HotSpot, `RSslLiveSession`'s `drainTrap` family: once the
/// response body is fully drained the connection goes back to the
/// `KeepAliveCache`, its delegate is released, and every CONNECTION-level
/// accessor throws `IllegalStateException: connection not yet open` again —
/// the same exception a never-handshaked connection throws. The SESSION object
/// the application already holds survives and stays valid; it is the
/// *connection's* view that is torn down. MEASURED on CratonVM
/// (`9ae371468`, `target-rel3`) the four rows that says:
///
/// ```text
/// drain.conn.cipherSuite.raises  = none  WANT java.lang.IllegalStateException
/// drain.conn.cipherSuite.message = none  WANT connection not yet open
/// drain.conn.sslSession.raises   = none  WANT java.lang.IllegalStateException
/// drain.conn.sslSession.message  = none  WANT connection not yet open
/// ```
///
/// This is the entry point that closes them, and it is deliberately the ONLY
/// thing this file can contribute to it: the place that knows a body has been
/// drained to EOF is the `https:` input stream in `http_url_connection.rs`.
///
/// THE CALL SITE HAS ARRIVED (G7-1 / G44-1 N2): `http_url_connection.rs`'s
/// `huc_live_bais_event` calls `https_recycle_carrier_by_key` on the response
/// stream's first `Eof` or `Close`, which calls
/// [`forget_https_carrier_session_by_key`] below; `disconnect()` and the other
/// `https_recycle_carrier` call sites reach it the same way. The transcript
/// above is the PRE-fix measurement this entry point exists to close, not the
/// current state — CratonVM now matches it.
///
/// It used to be the table's only eviction path, so an entry — and a global
/// root on one `SSLSession` — lived for as long as the VM did for any carrier
/// that was never recycled (never disconnected and never drained/closed). Since
/// gc-common w17-a the entry also goes when the carrier itself is collected
/// ([`HttpsCarrierSession::owner`], dropped by [`gc_sweep_net_socket_rows`]):
/// HotSpot's `KeepAliveCache` keeps the underlying HTTP client for reuse, never
/// the `HttpsURLConnection` object.
pub fn forget_https_carrier_session(ctx: &mut dyn NativeContext, connection: ObjectRef) {
    let key = existing_native_obj_key(&*ctx, connection);
    forget_https_carrier_session_by_key(ctx, key);
}

/// [`forget_https_carrier_session`] for a caller that has the KEY but not the
/// object.
///
/// The response-body drain observer is that caller. It is handed the
/// `ByteArrayInputStream` and nothing else, and the carrier it must recycle
/// may by then be unreachable from any root the observer can see — so the key
/// is captured when the stream is built and carried, rather than the object.
/// A `NativeObjKey` is two integers and holds nothing alive, which is exactly
/// why it can be stored across an arbitrary span of Java execution when an
/// `ObjectRef` could not.
///
/// `None` is a carrier that was never keyed here (gc-common w28-a: the key is
/// looked up, never minted, by [`existing_native_obj_key`]), which has no row:
/// nothing to forget.
pub fn forget_https_carrier_session_by_key(
    ctx: &mut dyn NativeContext,
    key: Option<NativeObjKey>,
) {
    let Some(key) = key else {
        return;
    };
    let stale = {
        let mut table = https_carrier_sessions().lock();
        table.remove(&key).map(|e| e.session_root).unwrap_or(0)
    };
    if stale != 0 {
        ctx.remove_global_root(stale);
    }
}

/// The recorded session for this carrier, CLONED out of the table before
/// anything else happens.
///
/// Cloning rather than holding the guard is load-bearing: every caller below
/// goes on to allocate Java objects and to `invoke_virtual` into Java, and a
/// native that holds a process-global lock across a call back into bytecode is
/// the lock cycle this project has already paid for once (`registry-guard
/// across a blocking call`).
pub fn https_carrier_session(
    ctx: &dyn NativeContext,
    connection: ObjectRef,
) -> Option<HttpsCarrierSession> {
    let key = existing_native_obj_key(ctx, connection)?;
    https_carrier_sessions().lock().get(&key).cloned()
}

/// HotSpot's exact refusal for any session accessor on a connection that has
/// not completed a handshake -- see the transcript above. A real, catchable
/// `java.lang.IllegalStateException`, not an `IOException` whose message merely
/// reads like one: callers catch this by type.
pub fn https_not_yet_open(ctx: &mut dyn NativeContext) -> MethodCallFailed {
    let jmsg = ctx.create_string("connection not yet open");
    match ctx.new_object_initialized(
        "java/lang/IllegalStateException",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(jmsg))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => {
            // The caller's Java frame has no catch-local root for this yet and
            // `new_object_initialized` has already released its constructor
            // pin -- same hand-off as `socket_ex`.
            let exc_pin = ctx.pin_native_root(exc);
            let exc = ctx.read_native_pin(exc_pin, exc);
            MethodCallFailed::ExceptionThrown(exc)
        }
        Ok(_) => ioex("connection not yet open"),
        Err(failed) => failed,
    }
}

/// The value BOTH HTTPS-client `SSLSession` minters write into
/// `ssl_security::NEW13_SESS_TLSID` — a **presence marker, never a lookup
/// key**.
///
/// F10. This slot used to hold `Int(-1)` on both of them, and `-1` is the
/// "never negotiated" sentinel: `t27_tls::session_has_negotiated` reads exactly
/// this slot and answers `slot2 >= 0`. Both minters are reached only from a
/// handshake that **completed**, so `-1` asserted the opposite of the truth.
/// While `session_has_negotiated`'s width-4 case fell into a catch-all
/// `_ => true` the lie was invisible; F6 merged that arm onto `3 | 4` — the
/// correct fix — and the lie became the answer. MEASURED, HotSpot 25.0.3+9-LTS
/// `Microsoft-13877124`, loopback `HttpsServer` + `HttpsURLConnection`, three
/// byte-identical runs (`scratchpad/f10/F10HttpsSession.java`):
///
/// ```text
///   completed HTTPS handshake   isValid=true  idLen=32 cipher=TLS_AES_256_GCM_SHA384 proto=TLSv1.3
///   never-connected SSLSocket   isValid=false idLen=0  cipher=SSL_NULL_WITH_NULL_NULL proto=NONE
/// ```
///
/// **Why a marker and not a real id.** The preferred fix is to register the
/// connection in the `servlet` TLS id space and write
/// `RUSTLS_SOCK_ID_BASE + stream_id`, as `t27_tls`'s `SSLServerSocket.accept`
/// does. That is not available here, and not merely because it is invasive:
/// this connection's rustls state is a `StreamOwned<ClientConnection,
/// TcpStream>` local to `http_url_connection::perform`, which **drops it before
/// any of these accessors run** — `https_session_object` is called from
/// `getSSLSession()`/`getPeerPrincipal()`/... long after `perform` returned, so
/// at that moment there is no stream to register. Registering the still-live
/// one from inside `huc_verify_hostname` would insert an entry that the HTTP
/// exchange then closes locally and that nothing ever removes (`s2_tls_close`
/// is the only remover, and it is never called for this stream): one leaked
/// registry entry per HTTPS request, each permanently answering "this stream is
/// alive". That trades a wrong boolean for an unbounded leak.
///
/// **Why THIS value, and why it is load-bearing.** Slot 2 is read as a lookup
/// key by registrations that are LIVE in real-JDK mode — in particular
/// `phases_late::ssl_security`'s `SSLSession.getPeerPrincipal`, which
/// `s2_tls_peer_cert_chain_der(tls_id)`s it (E22-1 §1's `--dump-native-registry`
/// table: `getPeerPrincipal` is owned by `ssl_security`, not `t27_tls`). So a
/// marker that could ever equal a live TLS stream id would hand one connection
/// **another connection's peer certificate chain** — a far worse failure than
/// the one being fixed. This value sits outside every id range in
/// `servlet.rs`: below `PENDING_CONNECT_SOCK_ID_BASE` (`0x1000_0000`),
/// `PENDING_LAYERED_SOCK_ID_BASE` (`0x2000_0000`) and `RUSTLS_SOCK_ID_BASE`
/// (`0x4000_0000`), and far above the small monotonic counter `s2_next_free_id`
/// hands out, so `s2_tls_session_info`/`s2_tls_peer_cert_chain_der` on it are
/// plain `HashMap` misses. It continues those three constants' halving
/// sequence and rests on the identical, already load-bearing assumption they
/// state: *"native-tls and rustls ids are small counters, so the high offset
/// never collides."*
///
/// **What a miss costs, stated rather than assumed:** nothing changes
/// direction. With `-1` those readers skipped the lookup and used an empty
/// chain; with this marker they perform the lookup, miss, and use an empty
/// chain. Same answer, same exception, for a reason that is now written down.
/// (That answer is itself wrong for a completed handshake — HotSpot measures
/// `getPeerPrincipal() == CN=localhost` — but it is wrong identically before
/// and after this change; see this file's F10 record, NOMINATION 2.)
///
/// **Deliberately ONE constant rather than a per-connection counter.**
/// `getId()`'s bytes do not come from this slot in either mode — `t27_tls`'s
/// real-mode copy seeds from `gc_stable_objref_key(session)` and `tls.rs`'s
/// `--synthetic-jdk` copy from `identity_hash_code(session)` — so per-session
/// distinctness is already carried by the session object's identity, and
/// varying this value would buy nothing while re-opening the collision surface
/// the paragraph above closes.
pub const HTTPS_CLIENT_SESSION_MARKER: i32 = 0x0800_0000;

/// Materialise the recorded handshake as the same synthetic
/// `javax/net/ssl/SSLSession` shape the hostname-verifier path builds, so the
/// layout-aware accessors already registered in
/// `t27_tls::register_ssl_session_real` read it correctly.
///
/// Deliberately NOT a second certificate decoder. `getPeerCertificates`,
/// `getPeerPrincipal` and `getLocalPrincipal` are already implemented once, on
/// `javax/net/ssl/SSLSession`; the accessors below delegate to them rather than
/// re-deriving an X.509 subject from DER, which is the "thin direct helper
/// reimplements the native" shape this workspace keeps finding.
/// ONE OBJECT PER CARRIER, not one per call. The first accessor to ask mints
/// the session, roots it globally and records the handle against the carrier;
/// every later ask — from any of the six accessors, and from
/// [`https_carrier_session_object`] — gets that same object back. See
/// [`HttpsCarrierSession::session_root`] for the measured row this closes.
pub fn https_session_object(
    ctx: &mut dyn NativeContext,
    connection: ObjectRef,
    s: &HttpsCarrierSession,
) -> Result<ObjectRef, MethodCallFailed> {
    if let Some(cached) = https_cached_session_object(&*ctx, connection) {
        return Ok(cached);
    }
    // GC: the carrier is read again AFTER the allocations below, to key the
    // table when the new session is published. A raw `ObjectRef` does not
    // survive a moving young GC, and `identity_hash_code` on a vacated
    // from-space address is not the identity of anything — so pin it first.
    // This pin must be taken BEFORE the session's, because releasing a pin
    // truncates the stack above it.
    let conn_pin = ctx.pin_native_root(connection);
    let session0 = crate::t27_tls::alloc_session_carrier(
        ctx,
        crate::phases_late::ssl_security::NEW13_SSL_SESS_FIELDS,
    )?;
    let session_pin = ctx.pin_native_root(session0);

    // GC: each allocated argument must survive the allocations that follow it,
    // so the session is re-read from its pin before every write.
    let proto_s = ctx.create_string(&s.protocol);
    let session = ctx.read_native_pin(session_pin, session0);
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        crate::phases_late::ssl_security::NEW13_SESS_PROTO,
        Value::Object(Some(proto_s)),
    );
    let cipher_s = ctx.create_string(&s.cipher);
    let session = ctx.read_native_pin(session_pin, session0);
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        crate::phases_late::ssl_security::NEW13_SESS_CIPHER,
        Value::Object(Some(cipher_s)),
    );
    // This session's handshake COMPLETED — `https_carrier_session` only has an
    // entry because `record_https_carrier_session` was called from the far side
    // of a successful TLS handshake — so slot 2 must not carry the "never
    // negotiated" sentinel. See `HTTPS_CLIENT_SESSION_MARKER` for the measured
    // contract, for why this connection cannot be given a real `servlet` TLS id,
    // and for why the marker's numeric value is not free to choose. Same value
    // and same reason as `http_url_connection::huc_verify_hostname`'s session:
    // the two minters share the constant so they cannot drift apart.
    crate::t27_tls::sess_set(
        &*ctx,
        session,
        crate::phases_late::ssl_security::NEW13_SESS_TLSID,
        Value::Int(HTTPS_CLIENT_SESSION_MARKER),
    );
    let session = ctx.read_native_pin(session_pin, session0);
    crate::t27_tls::record_client_peer_chain(ctx, session, s.peer_chain_der.clone());
    // G51-1 N1. `getPeerHost()`/`getPeerPort()` cannot be answered from this
    // shape or from the socket registry: slot 2 carries
    // `HTTPS_CLIENT_SESSION_MARKER`, a constant chosen SO THAT every
    // socket-registry lookup misses (see its doc comment for why this
    // connection cannot be given a real `servlet` TLS id without leaking a
    // registry entry per request). The fallback therefore misses by design,
    // and the side table is the only place the answer can live.
    //
    // `record_session_peer_endpoint` is a no-op on an empty host AND a
    // non-positive port, so a carrier recorded before this field existed —
    // or any future path that cannot name an endpoint — leaves the readers
    // falling through to `session_stream_id` exactly as they did before,
    // rather than being shadowed by a row of `("", -1)`.
    let session = ctx.read_native_pin(session_pin, session0);
    crate::t27_tls::record_session_peer_endpoint(ctx, session, &s.peer_host, s.peer_port);

    // Publish it as THE session for this carrier. The root is taken before the
    // table is touched, and the table is consulted under its own lock with no
    // allocation in between, so a second thread that minted concurrently loses
    // its object here rather than leaving two live sessions and a leaked root.
    let session = ctx.read_native_pin(session_pin, session0);
    let handle = ctx.add_global_root(session);
    let connection = ctx.read_native_pin(conn_pin, connection);
    // Looked up, never minted (gc-common w28-a): a carrier with no key has no
    // entry to publish into, the same outcome as a missing entry.
    let key = existing_native_obj_key(&*ctx, connection);
    let mut claimed = false;
    let mut winner = 0usize;
    if let Some(key) = key {
        let mut table = https_carrier_sessions().lock();
        if let Some(entry) = table.get_mut(&key) {
            if entry.session_root == 0 {
                entry.session_root = handle;
                claimed = true;
            } else {
                winner = entry.session_root;
            }
        }
    }
    if !claimed {
        ctx.remove_global_root(handle);
        if winner != 0 {
            if let Some(obj) = ctx.resolve_global_root(winner) {
                return Ok(obj);
            }
        }
    }
    Ok(ctx.read_native_pin(session_pin, session0))
}

/// The session object already minted for this carrier, or `None` if none has
/// been. Reads through the global-root handle, so the address is the collector's
/// current one even after a moving collection.
pub fn https_cached_session_object(
    ctx: &dyn NativeContext,
    connection: ObjectRef,
) -> Option<ObjectRef> {
    let key = existing_native_obj_key(ctx, connection)?;
    let handle = {
        let table = https_carrier_sessions().lock();
        table.get(&key).map(|e| e.session_root)?
    };
    if handle == 0 {
        return None;
    }
    ctx.resolve_global_root(handle)
}

/// THE `SSLSession` for a carrier whose handshake completed, for callers
/// OUTSIDE this file — specifically `http_url_connection::huc_verify_hostname`,
/// which currently mints a second, private one to hand to the application's
/// `HostnameVerifier`.
///
/// MEASURED on HotSpot (`RSslLiveSession`'s `verifier` family): the object a
/// `HostnameVerifier` is given and the object `getSSLSession()` returns
/// afterwards are the SAME object — `verifier.sameObjectAsGetSSLSession`. Two
/// minters cannot satisfy that no matter how identical their field writes are,
/// which is why the fix is one minter with an entry point rather than a second
/// copy of the same four `set_field` calls.
///
/// `None` means the carrier has no recorded handshake, i.e. the caller is
/// upstream of `record_https_carrier_session`; it is NOT an error.
pub fn https_carrier_session_object(
    ctx: &mut dyn NativeContext,
    connection: ObjectRef,
) -> Option<Result<ObjectRef, MethodCallFailed>> {
    let s = https_carrier_session(&*ctx, connection)?;
    Some(https_session_object(ctx, connection, &s))
}

/// The six `javax.net.ssl.HttpsURLConnection` session accessors, on BOTH
/// carrier classes.
///
/// Registered on the abstract base as well as the Impl because the base is
/// still reachable: `javax/net/ssl/HttpsURLConnection` remains a carrier for
/// anything that constructed one directly, and its three abstract declarations
/// have no Code attribute at all, so without a native there is nothing to run.
///
/// REGISTRATION ORDER -- and a correction. `register()` is last-write-wins.
/// The note that used to stand here checked ONE function, `http_url_connection
/// .rs`'s `register_one`, found none of these six names in it, and concluded
/// they could not be overwritten. That conclusion is FALSE, and the dump says
/// so. `http_url_connection.rs` holds a SECOND registrar with the same
/// function name as this one -- `register_https_session_accessors`, at
/// `http_url_connection.rs:404` -- and it runs later.
///
/// RE-MEASURED 2026-08-17 on `9964ca733` (`target-rel2`), after another lane
/// had edited `http_url_connection.rs`: the collapse is UNCHANGED, only the
/// line numbers moved. `--dump-native-registry` under `--real-jdk`, both
/// carrier classes, identical rows:
///
/// ```text
/// getCipherSuite         owns_slot=false  net_phase_e.rs:8439
/// getCipherSuite         owns_slot=TRUE   http_url_connection.rs:445  overwrote=bridge
/// getServerCertificates  owns_slot=false  net_phase_e.rs:8447
/// getServerCertificates  owns_slot=TRUE   http_url_connection.rs:405  overwrote=bridge
/// getLocalCertificates   owns_slot=false  net_phase_e.rs:8470
/// getLocalCertificates   owns_slot=TRUE   http_url_connection.rs:433  overwrote=bridge
/// getPeerPrincipal       owns_slot=false  net_phase_e.rs:8488
/// getPeerPrincipal       owns_slot=TRUE   http_url_connection.rs:466  overwrote=bridge
/// getLocalPrincipal      owns_slot=false  net_phase_e.rs:8501
/// getLocalPrincipal      owns_slot=TRUE   http_url_connection.rs:495  overwrote=bridge
/// getSSLSession          owns_slot=TRUE   net_phase_e.rs:8519
/// ```
///
/// `invocations` is 0 on every row of that dump, and that is NOT evidence of
/// death: the probe it was taken from makes no HTTPS request. The field that
/// settles ownership is `owns_slot`.
///
/// So FIVE of the six bodies below are dead: only `getSSLSession` runs, and
/// only because the other file does not register it. A change to any of the
/// other five here has no runtime effect at all -- it must be made in
/// `http_url_connection.rs::register_https_session_accessors` instead (that
/// file is not this lane's to edit; see the NOMINATION in
/// `G16-1-the-server-socket-impl-and-how-far-RSslLiveSession-got-20260817.md`).
///
/// The general lesson, which the old note had exactly backwards: a shadowing
/// registrar is not ruled out by grepping one function for one name. Only the
/// dump settles it. `owns_slot=true` with a non-zero `invocations` is proof;
/// source order is not.
pub fn register_https_session_accessors(r: &mut NativeMethodRegistry) {
    // Bridge, explicitly and locally: these stand in for real JDK bytecode that
    // exists and would work if `delegate` were populated. Set here rather than
    // inherited from whatever category the enclosing registration scope happens
    // to be in, so the `--dump-native-registry` kind for these six does not
    // depend on where the call sits.
            for cls in [
        "sun/net/www/protocol/https/HttpsURLConnectionImpl",
        "javax/net/ssl/HttpsURLConnection",
    ] {
        r.register(
            cls,
            "getCipherSuite",
            "()Ljava/lang/String;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let Some(s) = https_carrier_session(ctx, this) else {
                    return Err(https_not_yet_open(ctx));
                };
                let out = ctx.create_string(&s.cipher);
                Ok(Some(Value::Object(Some(out))))
            }, NativeKind::Bridge,
        );
        r.register(
            cls,
            "getServerCertificates",
            "()[Ljava/security/cert/Certificate;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let Some(s) = https_carrier_session(ctx, this) else {
                    return Err(https_not_yet_open(ctx));
                };
                let session = https_session_object(ctx, this, &s)?;
                // `SSLSession.getPeerCertificates` is the single implementation
                // of "decode this chain into java.security.cert.Certificate
                // mirrors", and it already throws SSLPeerUnverifiedException
                // for an empty chain -- the real-JDK contract, and the same one
                // HttpsURLConnection.getServerCertificates declares.
                ctx.invoke_virtual(
                    session,
                    "getPeerCertificates",
                    "()[Ljava/security/cert/Certificate;",
                    &[],
                )
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getLocalCertificates",
            "()[Ljava/security/cert/Certificate;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let Some(s) = https_carrier_session(ctx, this) else {
                    return Err(https_not_yet_open(ctx));
                };
                let session = https_session_object(ctx, this, &s)?;
                ctx.invoke_virtual(
                    session,
                    "getLocalCertificates",
                    "()[Ljava/security/cert/Certificate;",
                    &[],
                )
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getPeerPrincipal",
            "()Ljava/security/Principal;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let Some(s) = https_carrier_session(ctx, this) else {
                    return Err(https_not_yet_open(ctx));
                };
                let session = https_session_object(ctx, this, &s)?;
                ctx.invoke_virtual(
                    session,
                    "getPeerPrincipal",
                    "()Ljava/security/Principal;",
                    &[],
                )
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getLocalPrincipal",
            "()Ljava/security/Principal;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let Some(s) = https_carrier_session(ctx, this) else {
                    return Err(https_not_yet_open(ctx));
                };
                let session = https_session_object(ctx, this, &s)?;
                ctx.invoke_virtual(
                    session,
                    "getLocalPrincipal",
                    "()Ljava/security/Principal;",
                    &[],
                )
            }, cratonvm_native_api::NativeKind::Bridge,
        );
        r.register(
            cls,
            "getSSLSession",
            "()Ljava/util/Optional;",
            |ctx, args| {
                let this = obj_arg(args, 0)?;
                let Some(s) = https_carrier_session(ctx, this) else {
                    // NOT `Optional.empty()`. HotSpot throws here too (transcript
                    // above), and the empty Optional is precisely the silent lie
                    // this change exists to remove.
                    return Err(https_not_yet_open(ctx));
                };
                let session = https_session_object(ctx, this, &s)?;
                let session_pin = ctx.pin_native_root(session);
                // `java.util.Optional` has exactly one instance field, `value`, at
                // slot 0 -- this is `Optional.ofNullable(session)` in the real
                // layout, not the two-slot flag+value shape `http2.rs` uses.
                let opt = try_alloc_concurrent_synthetic(ctx, "java/util/Optional", 1)?;
                let session = ctx.read_native_pin(session_pin, session);
                ctx.set_field(opt, 0, Value::Object(Some(session)));
                Ok(Some(Value::Object(Some(opt))))
            }, cratonvm_native_api::NativeKind::Bridge,
        );
    }
}

