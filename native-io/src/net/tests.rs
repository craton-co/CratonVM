use super::*;
use crate::test_support::MockNativeContext;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::{Read as _, Write as _};
use std::thread;

// Most tests operate directly on the internal helpers rather than through
// the registry -> NativeContext dispatch machinery, because standing up a
// MockNativeContext with a live FileDescriptor layout for each test would
// swamp the file. Tests for the end-to-end path live in the VM
// integration suite.

fn make_fd_with_id(id: i32) -> i32 {
    net_sockets().write().insert(id, NetSocketHandle::Unbound);
    id
}

fn remove_fd(id: i32) {
    net_sockets().write().remove(&id);
}

/// nio20 wave 5 (L4): a live socket's refused option throws libnio's
/// `SocketException` at every level but `IPPROTO_IPV6`, whose refusal is
/// the VM's `AF_INET` socket under a dual-stack JDK channel.
#[test]
fn live_option_failures_throw_except_at_the_ipv6_level() {
    assert!(live_option_failure_throws(sockopt_sys::SOL_SOCKET));
    assert!(live_option_failure_throws(sockopt_sys::IPPROTO_IP));
    assert!(live_option_failure_throws(IPPROTO_TCP));
    assert!(!live_option_failure_throws(NET_IPPROTO_IPV6));
}

/// `close_if_net_descriptor` is the JDK 17 `SocketDispatcher.close` path
/// under `--real-jdk` (`FileDispatcherImpl.close0` on a socket id). It must
/// close exactly what `net_close` closes, and must leave a descriptor that
/// names no `net_sockets()` id (a FILE table id) to the file close.
#[test]
fn close_if_net_descriptor_closes_only_a_net_id() {
    let mut ctx = MockNativeContext::new();
    let fd = register_handle(NetSocketHandle::Unbound);
    let fd_obj = ctx.alloc_object(4);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
    assert!(close_if_net_descriptor(&mut ctx, fd_obj));
    assert_eq!(_test_peek_kind(fd), "closed");
    assert!(matches!(
        ctx.get_field_by_name(fd_obj, "fd"),
        Value::Int(-1)
    ));

    let file_obj = ctx.alloc_object(4);
    ctx.set_field_by_name(file_obj, "fd", Value::Int(7));
    assert!(!close_if_net_descriptor(&mut ctx, file_obj));
    assert!(matches!(
        ctx.get_field_by_name(file_obj, "fd"),
        Value::Int(7)
    ));
    remove_fd(fd);
}

/// A listener poll must not hold the registry read guard while it polls.
///
/// The bug this pins (2026-08-05) hung the VM outright: `net_poll` returned
/// `net_poll_listener(..)` from inside a `net_sockets().read()` block, so
/// the guard outlived the call, and the poll loop's own
/// `net_listener_still_registered` took the same read lock again. With
/// `parking_lot`'s fair `RwLock`, a `socket0` (`register_handle`, a write)
/// arriving in between wedges all three.
///
/// The test asserts the property directly and with a BOUND, so a
/// regression fails in a second instead of hanging the suite the way the
/// defect hangs the VM: while a listener poll is in flight, a writer must
/// still be able to take the registry lock.
#[test]
fn a_listener_poll_does_not_hold_the_registry_lock() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    listener.set_nonblocking(false).expect("blocking listener");
    let fd = register_handle(NetSocketHandle::Listener(Arc::new(Mutex::new(listener))));
    // Poll for a connection that never comes, on another thread, THROUGH
    // `net_poll` — the entry point that held the guard. Calling
    // `net_poll_listener` directly proves nothing: the defect is the
    // caller's guard, so a direct call passes on the broken code too
    // (it did, on the first attempt at this test).
    let poller = thread::spawn(move || {
        let mut ctx = MockNativeContext::new();
        let fd_obj = ctx.alloc_object(4);
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
        // The JDK's `Net` POLLIN differs per platform and this file's
        // poll shims take it verbatim: `poll(2)`'s 1 on Unix, WSAPoll's
        // 0x300 on Windows. Passing the wrong one makes the poll fail
        // instantly with EINVAL — which looked exactly like a passing
        // test, since a poll that never blocks holds no guard.
        #[cfg(windows)]
        const POLLIN: i32 = 0x300;
        #[cfg(not(windows))]
        const POLLIN: i32 = 1;
        let t0 = std::time::Instant::now();
        let r = net_poll(
            &mut ctx,
            &[
                Value::Object(Some(fd_obj)),
                Value::Int(POLLIN),
                Value::Long(1_500),
            ],
        );
        (r.is_ok(), t0.elapsed())
    });

    thread::sleep(Duration::from_millis(250));

    // The assertion: a writer gets in. Pre-fix this returns `None` — the
    // outer read guard is still held and the writer is queued behind it,
    // which is exactly the state the deadlock freezes.
    let acquired = net_sockets()
        .try_write_for(Duration::from_millis(750))
        .is_some();

    let (poll_ok, poll_took) = poller.join().expect("poll thread");
    remove_fd(fd);
    // A poll that returned instantly holds no guard and so cannot observe
    // the defect. Say so instead of passing: this test reported a clean
    // pass against the KNOWN-BROKEN code twice before this assertion
    // existed — once because it called `net_poll_listener` directly, once
    // because it passed Unix's POLLIN on Windows.
    assert!(
        poll_ok && poll_took >= Duration::from_millis(400),
        "the poll did not block (ok={poll_ok}, took={poll_took:?}), so this \
         test could not exercise the registry guard at all"
    );
    assert!(
        acquired,
        "a writer could not take the socket registry while a listener poll \
         was in flight — the poll is holding the read guard across its loop, \
         and its own nested read will deadlock against the queued writer"
    );
}

/// `configureBlocking` must not hold the registry read guard while it waits
/// for a listener's mutex — the other half of the cycle the poll test above
/// closes.
///
/// `net_accept` holds `listener_handle.lock()` across
/// `net_listener_still_registered`, i.e. listener-mutex -> registry-read,
/// for the whole of a blocking accept. `configureBlocking` took the
/// opposite order. Two orders is a cycle, and a fair `RwLock` closes it as
/// soon as any writer queues.
///
/// Unlike its sibling, this one CAN call the handler directly: the guard is
/// inside `net_configure_blocking` itself, not inherited from a caller. The
/// rest of that test's hard-won shape is kept — assert the path was actually
/// exercised (this thread really did wait on the mutex) before trusting the
/// writer's result, so "could not reach the path" can never read as a pass —
/// and every wait is bounded, so a regression fails in a second instead of
/// hanging the suite the way the defect hangs the VM.
#[test]
fn configure_blocking_does_not_hold_the_registry_lock_waiting_for_a_listener() {
    assert_configure_blocking_without_registry_lock(false);
}

#[test]
fn configure_blocking_racing_close_does_not_restore_retired_mode() {
    assert_configure_blocking_without_registry_lock(true);
}

fn assert_configure_blocking_without_registry_lock(close_during_configure: bool) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let handle = Arc::new(Mutex::new(listener));
    let fd = register_handle(NetSocketHandle::Listener(Arc::clone(&handle)));

    // Stand in for a blocking `net_accept`: hold the listener mutex for
    // longer than the writer below is willing to wait.
    let held = Arc::clone(&handle);
    let holder = thread::spawn(move || {
        let guard = held.lock();
        thread::sleep(Duration::from_millis(1_200));
        drop(guard);
    });
    thread::sleep(Duration::from_millis(100));

    let configurer = thread::spawn(move || {
        let mut ctx = MockNativeContext::new();
        let fd_obj = ctx.alloc_object(4);
        ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
        let t0 = std::time::Instant::now();
        let r = net_configure_blocking(&mut ctx, &[Value::Object(Some(fd_obj)), Value::Int(0)]);
        (r.is_ok(), t0.elapsed())
    });

    thread::sleep(Duration::from_millis(300));
    let nonblocking_while_waiting = net_fd_is_nonblocking(fd);

    // The assertion: a registry writer gets in while `configureBlocking` is
    // parked on the listener mutex. Pre-fix it is queued behind the read
    // guard that call is still holding — the state the deadlock freezes.
    let acquired = net_sockets()
        .try_write_for(Duration::from_millis(600))
        .is_some();

    if acquired && close_during_configure {
        close_net_fd(fd);
    }
    let (ok, took) = configurer.join().expect("configureBlocking thread");
    holder.join().expect("mutex holder");
    let nonblocking_after_configure = net_fd_is_nonblocking(fd);
    close_net_fd(fd);
    remove_fd(fd);
    assert!(!nonblocking_while_waiting);
    assert_eq!(nonblocking_after_configure, !close_during_configure);

    assert!(
        ok && took >= Duration::from_millis(400),
        "configureBlocking did not wait for the listener mutex (ok={ok}, \
         took={took:?}), so this test could not exercise the registry guard"
    );
    assert!(
        acquired,
        "a writer could not take the socket registry while configureBlocking \
         was waiting for a listener mutex — it is holding the read guard \
         across that wait, which deadlocks against an accept holding the \
         mutex and reaching for the same read lock"
    );
}

#[test]
fn t19_5_socket0_returns_nonzero_fd() {
    // Two allocations produce distinct, nonzero fds.
    let a = register_handle(NetSocketHandle::Unbound);
    let b = register_handle(NetSocketHandle::Unbound);
    assert_ne!(a, 0);
    assert_ne!(b, 0);
    assert_ne!(a, b);
    assert_eq!(_test_peek_kind(a), "unbound");
    remove_fd(a);
    remove_fd(b);
}

#[test]
fn t19_5_bind0_ipv4_loopback_succeeds() {
    // Direct binding via TcpListener + our registry.
    let listener = TcpListener::bind("127.0.0.1:0").expect("loopback bind");
    let local = listener.local_addr().unwrap();
    let fd = register_handle(NetSocketHandle::Listener(Arc::new(Mutex::new(listener))));
    assert_eq!(_test_peek_kind(fd), "listener");
    assert!(local.port() > 0, "ephemeral port should be > 0");
    remove_fd(fd);
}

#[test]
fn t19_5_bind0_port_already_in_use_throws_bind_exception() {
    let first = TcpListener::bind("127.0.0.1:0").expect("first bind");
    let port = first.local_addr().unwrap().port();
    // Second bind at same port fails with AddrInUse or AddrNotAvailable.
    let err = TcpListener::bind(format!("127.0.0.1:{port}")).unwrap_err();
    let mcf = net_err_typed(&err);
    let s = format!("{mcf:?} {:?}", err.kind());
    assert!(
        s.contains("BindException") || s.contains("AddrInUse"),
        "expected BindException in {s}"
    );
    drop(first);
}

/// `Net.listen` used to be a no-op that answered `Ok` to anything, on the
/// grounds that `TcpListener::bind` had already listened — at std's 128,
/// whatever backlog the JDK passed. `bind0` now leaves the socket unlistened
/// and this native is where the backlog is applied, so a call that names no
/// socket must fail instead of silently succeeding. The backlog itself is
/// pinned against real sockets in `cratonvm_native_api::net_wait`'s
/// `deferred_listen_on_an_unlistened_listener_applies_the_backlog`.
#[test]
fn t19_5_listen_without_a_descriptor_is_an_error_not_a_silent_success() {
    let mut ctx = MockNativeContext::new();
    let r = net_listen(&mut ctx, &[]);
    if cratonvm_native_api::net_wait::jdk_backlog_enabled() {
        assert!(r.is_err(), "listen with no FileDescriptor answered {r:?}");
    } else {
        assert!(matches!(r, Ok(None)));
    }
}

#[test]
fn t19_5_accept_blocks_until_connection() {
    // Bind a server, spawn a client that connects, then accept().
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();

    let client_thread = thread::spawn(move || {
        // Retry briefly to avoid races with the server readying.
        for _ in 0..50 {
            if let Ok(s) = TcpStream::connect(("127.0.0.1", port)) {
                return s;
            }
            thread::sleep(Duration::from_millis(10));
        }
        panic!("client connect failed");
    });

    let (stream, peer) = server.accept().expect("accept");
    let _s = client_thread.join().expect("client join");
    assert_eq!(peer.ip().to_string(), "127.0.0.1");
    // Stream is live — write+close without blocking.
    drop(stream);
}

#[test]
fn t19_5_connect0_to_local_listener_succeeds() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();

    // Accept in background so connect() completes.
    let srv = thread::spawn(move || {
        let _ = server.accept();
    });

    let client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    assert_eq!(client.peer_addr().unwrap().port(), port);
    drop(client);
    let _ = srv.join();
}

#[test]
fn t19_5_read_write_round_trip() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();

    let srv = thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
        s.write_all(b"world").unwrap();
    });

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    client.write_all(b"hello").unwrap();
    let mut buf = [0u8; 5];
    client.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"world");
    srv.join().unwrap();
}

#[test]
fn t19_5_write0_that_fits_needs_no_blocking_region() {
    assert_write0_without_blocking_region(true);
}

#[test]
fn nonblocking_write0_needs_no_blocking_region() {
    assert_write0_without_blocking_region(false);
}

fn assert_write0_without_blocking_region(blocking: bool) {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();

    let srv = thread::spawn(move || {
        let (mut s, _) = server.accept().unwrap();
        let mut buf = [0u8; 5];
        s.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(client)));
    let mut ctx = MockNativeContext::new();
    let fd_obj = ctx.alloc_object(0);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
    if !blocking {
        net_configure_blocking(&mut ctx, &[Value::Object(Some(fd_obj)), Value::Int(0)])
            .unwrap();
    }

    let bytes = *b"hello";
    let result = net_write0(
        &mut ctx,
        &[
            Value::Object(Some(fd_obj)),
            Value::Long(bytes.as_ptr() as i64),
            Value::Int(bytes.len() as i32),
        ],
    )
    .unwrap();

    assert_eq!(result, Some(Value::Int(bytes.len() as i32)));
    // Five bytes fit the empty send buffer: since SBR wave 5 (lane
    // `nioreal`) they go out in one non-blocking `send` with NO GC-blocking
    // region -- a write that cannot park needs none. A write that must
    // park still brackets its wait (`net_stream_write`'s parking arm).
    // Windows has the same fast path since 2026-10-04 (`send_dontwait`: a
    // zero-timeout `WSAPoll`, then one slice-sized `send`).
    srv.join().unwrap();
    close_net_fd(fd);
    remove_fd(fd);
    assert_eq!(ctx.blocking_region_counts(), (0, 0));
}

#[test]
fn t19_5_connect0_applies_nonblocking_requested_while_fd_was_unbound() {
    // Regression for the JettyServletWebServerFactoryTests blocking-read
    // timeout hang: `NioSocketImpl.connect(timeout)` calls
    // `IOUtil.configureBlocking(fd, false)` BEFORE `Net.connect0`, while
    // the fd is still `Unbound` (no live socket to apply it to). That
    // request must survive the Unbound -> Stream transition, or the
    // freshly connected stream comes up in (default) blocking mode and
    // every later `read0` blocks for real instead of returning
    // `IOStatus.UNAVAILABLE`, defeating the JDK's SO_TIMEOUT protocol.
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();
    let srv = thread::spawn(move || {
        let (s, _) = server.accept().unwrap();
        // Deliberately never write or close - a genuinely blocking read0
        // on the client side would hang here instead of returning -2.
        thread::sleep(Duration::from_millis(300));
        drop(s);
    });

    let fd = register_handle(NetSocketHandle::Unbound);
    // Simulate the `configureBlocking(fd, false)` native call landing
    // while `fd` is still `Unbound` — exactly what the registered
    // `sun/nio/ch/IOUtil.configureBlocking` handler records.
    net_pending_nonblocking().write().insert(fd, true);

    let mut ctx = MockNativeContext::new();
    let fd_obj = ctx.alloc_object(0);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
    let addr_obj = ctx.alloc_object(2);
    let host_str = ctx.create_string("127.0.0.1");
    ctx.set_field(addr_obj, 1, Value::Object(Some(host_str)));

    let result = net_connect0(
        &mut ctx,
        &[
            Value::Int(0),
            Value::Object(Some(fd_obj)),
            Value::Object(Some(addr_obj)),
            Value::Int(port as i32),
        ],
    )
    .unwrap();
    // On Unix a socket the JDK put in non-blocking mode connects
    // asynchronously (`net_connect0`'s `nb_connect` arm, commit
    // `54d8ed53d8`): HotSpot's `connect0` answers `IOStatus.UNAVAILABLE`
    // (-2) for EINPROGRESS, and a loopback connect may also complete at
    // once (1). This test pinned the old synchronous answer, 1, and went red
    // on Linux when that arm landed (nio20 wave 1 L4 fix-back). Either is
    // HotSpot's; give an in-progress loopback handshake a moment to land.
    // Windows answers -2 always (a non-blocking connect there is always in progress).
    assert!(
        matches!(result, Some(Value::Int(1)) | Some(Value::Int(-2))),
        "{result:?}"
    );
    if result == Some(Value::Int(-2)) {
        thread::sleep(Duration::from_millis(50));
    }

    // The request is RETAINED, not consumed: it is the record of the fd's
    // blocking mode, which `net_accept` reads to decide between the JDK's
    // non-blocking accept contract and a genuine blocking accept. It must
    // not dangle past the fd, though — `close_net_fd` drops it (asserted
    // below).
    assert_eq!(net_pending_nonblocking().read().get(&fd), Some(&true));

    // And actually applied: reading with no data available must report
    // IOStatus.UNAVAILABLE (-2) promptly rather than blocking.
    let mut buf = [0u8; 8];
    let addr = buf.as_mut_ptr() as i64;
    let read_result = net_read0(
        &mut ctx,
        &[
            Value::Object(Some(fd_obj)),
            Value::Long(addr),
            Value::Int(buf.len() as i32),
        ],
    )
    .unwrap();
    assert_eq!(read_result, Some(Value::Int(-2)));

    // Closing the fd is what drops the retained mode.
    close_net_fd(fd);
    assert!(net_pending_nonblocking().read().get(&fd).is_none());

    remove_fd(fd);
    srv.join().unwrap();
}

#[test]
fn accept_on_a_nonblocking_listener_reports_unavailable_instead_of_blocking() {
    // `NioSocketImpl.timedAccept` flips the fd non-blocking and then needs
    // `Net.accept` to answer IOStatus.UNAVAILABLE (-2) so it can check its
    // own deadline. Blocking here instead made SO_TIMEOUT inert and
    // `ServerSocket.accept()` hang forever on an idle port.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = register_handle(NetSocketHandle::Listener(Arc::new(Mutex::new(listener))));
    net_pending_nonblocking().write().insert(fd, true);

    let mut ctx = MockNativeContext::new();
    let fd_obj = ctx.alloc_object(0);
    ctx.set_field_by_name(fd_obj, "fd", Value::Int(fd));
    let newfd_obj = ctx.alloc_object(0);

    let started = std::time::Instant::now();
    let result = net_accept(
        &mut ctx,
        &[
            Value::Object(Some(fd_obj)),
            Value::Object(Some(newfd_obj)),
            Value::Object(None),
        ],
    )
    .unwrap();
    assert_eq!(result, Some(Value::Int(-2)));
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "a non-blocking accept must not park"
    );

    close_net_fd(fd);
    remove_fd(fd);
}

#[test]
fn t19_5_setIntOption_so_reuseaddr_accepts_both_sides() {
    // Register a dummy fd, then set/get SO_REUSEADDR through the opt map.
    let fd = make_fd_with_id(0x5000_0001);
    net_opt_record(fd, SOL_SOCKET, SO_REUSEADDR, 1);
    assert_eq!(net_opt_recall(fd, SOL_SOCKET, SO_REUSEADDR), Some(1));
    // Also set to 0 to confirm toggling.
    net_opt_record(fd, SOL_SOCKET, SO_REUSEADDR, 0);
    assert_eq!(net_opt_recall(fd, SOL_SOCKET, SO_REUSEADDR), Some(0));
    remove_fd(fd);
    net_opt_forget_fd(fd);
}

/// A closed fd must not leave its options behind.
///
/// The store used to be keyed by `(fd, level, opt)` and nothing ever
/// removed an entry, so a long-lived VM accumulated one row per option per
/// socket it had EVER opened, forever.
#[test]
fn net_opts_are_dropped_when_the_socket_closes() {
    let fd = make_fd_with_id(0x5000_0011);
    net_opt_record(fd, IPPROTO_TCP, TCP_NODELAY, 1);
    assert_eq!(net_opt_snapshot(fd), vec![(IPPROTO_TCP, TCP_NODELAY, 1)]);
    close_net_fd(fd);
    assert_eq!(net_opt_recall(fd, IPPROTO_TCP, TCP_NODELAY), None);
    assert!(net_opt_snapshot(fd).is_empty());
    remove_fd(fd);
}

/// An option nobody ever set must not read back as 0 on a socket that has
/// no OS descriptor yet.
///
/// This is `RJdkNet.loopbackTcp`'s `SO_RCVBUF is positive` assertion: the
/// test calls `getReceiveBufferSize()` on a `new Socket()` that was never
/// connected, so `socket0` has handed out an id with no OS socket behind
/// it. HotSpot answers the kernel default; we used to answer 0.
#[test]
#[cfg(any(unix, windows))]
fn so_rcvbuf_default_is_positive_without_a_live_socket() {
    // The platform's own SO_RCVBUF numbering — the same pair the JDK's
    // generated SocketOptionRegistry hands the native.
    #[cfg(windows)]
    const SO_RCVBUF: i32 = 0x1002;
    #[cfg(unix)]
    const SO_RCVBUF: i32 = libc::SO_RCVBUF as i32;

    let level = sockopt_sys::SOL_SOCKET;
    let probed = net_default_int_option(level, SO_RCVBUF);
    assert!(
        matches!(probed, Some(v) if v > 0),
        "a fresh socket must report a positive SO_RCVBUF, got {probed:?}"
    );
}

#[test]
fn t19_5_getIntOption_returns_same_value_after_set() {
    let fd = make_fd_with_id(0x5000_0002);
    // Simulate setIntOption0 writing directly to the opts map.
    net_opt_record(fd, IPPROTO_TCP, TCP_NODELAY, 1);
    let out = net_opt_recall(fd, IPPROTO_TCP, TCP_NODELAY).unwrap_or(-1);
    assert_eq!(out, 1);
    remove_fd(fd);
    net_opt_forget_fd(fd);
}

#[test]
fn t19_5_localPort_reports_actual_bound_port() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let expected = listener.local_addr().unwrap().port();
    let fd = register_handle(NetSocketHandle::Listener(Arc::new(Mutex::new(listener))));
    let actual = {
        let map = net_sockets().read();
        match map.get(&fd) {
            Some(NetSocketHandle::Listener(l)) => l.lock().local_addr().unwrap().port(),
            _ => 0,
        }
    };
    assert_eq!(actual, expected);
    assert!(actual > 0);
    remove_fd(fd);
}

#[test]
fn t19_5_remoteInetAddress_after_connect() {
    let server = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = server.local_addr().unwrap().port();

    let srv = thread::spawn(move || {
        let (_s, _) = server.accept().unwrap();
        thread::sleep(Duration::from_millis(50));
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let peer = client.peer_addr().unwrap();
    assert_eq!(peer.ip().to_string(), "127.0.0.1");
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(client)));
    // Confirm the handle is indexed as a Stream.
    assert_eq!(_test_peek_kind(fd), "stream");
    remove_fd(fd);
    srv.join().unwrap();
}

#[test]
fn t19_5_isIPv6Available_matches_platform() {
    // The helper just checks whether binding to ::1:0 succeeds. Assert that
    // it matches the same check performed inline — i.e. the native is a
    // pure wrapper.
    let inline = TcpListener::bind("[::1]:0").is_ok();
    let mut ctx = MockNativeContext::new();
    let r = net_is_ipv6_available(&mut ctx, &[]).unwrap().unwrap();
    let v = match r {
        Value::Int(i) => i != 0,
        _ => panic!("expected Int"),
    };
    assert_eq!(v, inline);
}

#[test]
fn t19_5_close_removes_handle() {
    let fd = register_handle(NetSocketHandle::Unbound);
    assert_eq!(_test_peek_kind(fd), "unbound");
    // Call close via the internal API (bypasses FileDescriptor obj).
    close_net_fd(fd);
    assert_eq!(_test_peek_kind(fd), "closed");
    remove_fd(fd);
    assert_eq!(_test_peek_kind(fd), "missing");
}

#[test]
fn t19_5_accept_observes_close_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let handle = Arc::new(Mutex::new(listener));
    let fd = register_handle(NetSocketHandle::Listener(Arc::clone(&handle)));

    let waiter = thread::spawn(move || {
        let listener = handle.lock();
        let start = std::time::Instant::now();
        let err = net_accept_close_aware(&listener, fd).unwrap_err();
        (err.kind(), start.elapsed())
    });

    thread::sleep(Duration::from_millis(50));
    close_net_fd(fd);
    let (kind, elapsed) = waiter.join().unwrap();
    assert_eq!(kind, ErrorKind::Interrupted);
    assert!(
        elapsed < Duration::from_secs(2),
        "close-aware Net.accept should wake promptly, got {elapsed:?}"
    );
    remove_fd(fd);
}

#[test]
fn t19_5_close_unblocks_blocking_stream_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    let waiter = thread::spawn(move || {
        let (mut server_side, _) = listener.accept().unwrap();
        let mut buf = [0_u8; 1];
        let start = std::time::Instant::now();
        let result = server_side.read(&mut buf);
        (result, start.elapsed())
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(client)));
    thread::sleep(Duration::from_millis(50));
    close_net_fd(fd);

    let (result, elapsed) = waiter.join().unwrap();
    match result {
        Ok(n) => assert_eq!(n, 0, "shutdown should surface as EOF, got {n} bytes"),
        Err(e) => assert!(
            matches!(
                e.kind(),
                ErrorKind::ConnectionAborted
                    | ErrorKind::ConnectionReset
                    | ErrorKind::NotConnected
                    | ErrorKind::BrokenPipe
            ),
            "unexpected read error after close: {e:?}"
        ),
    }
    assert!(
        elapsed < Duration::from_secs(2),
        "close should wake blocked stream read promptly, got {elapsed:?}"
    );
    remove_fd(fd);
}

/// The half `t19_5_close_unblocks_blocking_stream_read` above does NOT
/// cover, and the one `RJdkNet.soTimeoutAndAsyncClose` asserts: that test
/// registers the CLIENT and then asserts the SERVER's read wakes, i.e. it
/// only proves the PEER observes the shutdown. The defect is the other
/// direction — a reader parked on the very fd being closed, in this
/// process. `shutdown` alone does not break that read on Windows, and the
/// OS handle cannot be closed while the reader holds an `Arc` clone of it.
#[test]
fn close_wakes_a_reader_parked_on_the_fd_being_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Hold the accepted end open and silent so the reader genuinely parks.
    let keeper = thread::spawn(move || listener.accept().unwrap().0);

    let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let fd = register_handle(NetSocketHandle::Stream(Arc::clone(&client)));

    let reader = thread::spawn(move || {
        let mut buf = [0_u8; 16];
        let start = std::time::Instant::now();
        let result = net_read_close_aware(fd, &client, &mut buf);
        (result.map_err(|e| e.kind()), start.elapsed())
    });

    thread::sleep(Duration::from_millis(100));
    close_net_fd(fd);

    let (result, elapsed) = reader.join().unwrap();
    assert_eq!(
        result.unwrap_err(),
        ErrorKind::Interrupted,
        "an asynchronous close must break the parked read"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "close should wake the parked reader promptly, got {elapsed:?}"
    );
    remove_fd(fd);
    drop(keeper.join().unwrap());
}

/// The other half of the same contract: a close-aware read must still be a
/// read. Payload that arrives while the reader is parked has to come back
/// on the very next pass, not after a poll timeout.
#[test]
fn close_aware_read_still_delivers_bytes_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let writer = thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(100));
        peer.write_all(b"ping").unwrap();
        peer
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(
        client.try_clone().unwrap(),
    )));
    let mut buf = [0_u8; 16];
    let n = net_read_close_aware(fd, &client, &mut buf).expect("read must succeed");
    assert_eq!(&buf[..n], b"ping");
    remove_fd(fd);
    drop(writer.join().unwrap());
}

/// The OTHER park site of the same contract, and the one
/// `RChannelInterrupt` exercises. The reader test above covers a thread
/// parked in `read0`; a socket whose
/// `connect` carried a timeout is left NON-blocking by `NioSocketImpl`
/// forever, so its untimed `read()` never reaches `read0`'s park at all —
/// it parks in `Net.poll(fd, POLLIN, -1)` instead. Before this test that
/// park was a single infinite `WSAPoll`/`poll` with no registry re-check,
/// so `Socket.close()` on another thread never reached it.
#[test]
fn close_wakes_a_stream_poll_parked_with_no_timeout() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Hold the accepted end open and silent, so the only way the poll can
    // return is by observing the close.
    let keeper = thread::spawn(move || listener.accept().unwrap().0);

    let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let fd = register_handle(NetSocketHandle::Stream(Arc::clone(&client)));

    let poller = thread::spawn(move || {
        let start = std::time::Instant::now();
        // -1 is what `NioSocketImpl.park(fd, event)` passes for an untimed
        // read: `nanos == 0` becomes `millis = -1`.
        let result = net_poll_stream_close_aware(fd, &client, NET_POLLIN, -1);
        (result.map_err(|e| e.kind()), start.elapsed())
    });

    thread::sleep(Duration::from_millis(100));
    close_net_fd(fd);

    let (result, elapsed) = poller.join().unwrap();
    assert!(
        result.unwrap(),
        "a deregistered fd must report ready, so the caller's next read \
         raises the concrete closed-socket error"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "close should wake the parked Net.poll promptly, got {elapsed:?}"
    );
    remove_fd(fd);
    drop(keeper.join().unwrap());
}

/// Slicing the wait must not cost readiness latency: payload that arrives
/// while the poll is parked has to be reported on the very next pass.
#[test]
fn a_close_aware_stream_poll_reports_real_readiness_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let writer = thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        thread::sleep(Duration::from_millis(100));
        peer.write_all(b"ping").unwrap();
        peer
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(
        client.try_clone().unwrap(),
    )));
    let start = std::time::Instant::now();
    let ready =
        net_poll_stream_close_aware(fd, &client, NET_POLLIN, -1).expect("poll must succeed");
    let elapsed = start.elapsed();
    assert!(ready, "the stream became readable, the poll must say so");
    assert!(
        elapsed < Duration::from_secs(2),
        "readiness must not wait out a slice budget, got {elapsed:?}"
    );
    remove_fd(fd);
    drop(writer.join().unwrap());
}

/// A zero timeout is a readiness PROBE, not a wait. The deadline loop must
/// not swallow it: `Instant::now() + 0ms` is already expired on the first
/// pass, which would report "not ready" without ever asking the OS.
#[test]
fn a_zero_timeout_stream_poll_probes_instead_of_reporting_not_ready() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let writer = thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"ping").unwrap();
        peer
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(
        client.try_clone().unwrap(),
    )));
    // Give the peer's bytes time to land, then probe with timeout 0.
    let mut ready = false;
    for _ in 0..200 {
        ready = net_poll_stream_close_aware(fd, &client, NET_POLLIN, 0)
            .expect("probe must succeed");
        if ready {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(
        ready,
        "a zero-timeout poll on a readable stream must answer ready"
    );
    remove_fd(fd);
    drop(writer.join().unwrap());
}

/// A BOUNDED wait still honours its own deadline. Slicing recomputes the
/// remaining time on every pass, so a timed poll on a silent socket must
/// return "not ready" at roughly its timeout — not early (which would break
/// `NioSocketImpl.timedRead`'s SO_TIMEOUT accounting) and not forever.
#[test]
fn a_timed_stream_poll_returns_not_ready_at_its_deadline() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let keeper = thread::spawn(move || listener.accept().unwrap().0);

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let fd = register_handle(NetSocketHandle::Stream(Arc::new(
        client.try_clone().unwrap(),
    )));
    let start = std::time::Instant::now();
    let ready =
        net_poll_stream_close_aware(fd, &client, NET_POLLIN, 300).expect("poll must succeed");
    let elapsed = start.elapsed();
    assert!(!ready, "a silent socket is not readable");
    assert!(
        elapsed >= Duration::from_millis(250),
        "the slices must not cut the caller's timeout short, got {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(5),
        "a bounded poll must end at its deadline, got {elapsed:?}"
    );
    remove_fd(fd);
    drop(keeper.join().unwrap());
}

#[test]
fn t19_5_shutdown_is_noop_on_listener() {
    // shutdown() on a listener fd should not panic — it's a no-op.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let fd = register_handle(NetSocketHandle::Listener(Arc::new(Mutex::new(listener))));
    // Simulate the inner match of net_shutdown without needing
    // a FileDescriptor object.
    let map = net_sockets().read();
    match map.get(&fd) {
        Some(NetSocketHandle::Stream(s)) => {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        _ => {}
    }
    drop(map);
    remove_fd(fd);
}

/// `isReusePortAvailable0` answers libnio's value (`true` on Linux, since SBR
/// wave 6 lane `asyncnet`); `canIPv6SocketJoinIPv4Group0`
/// answers libnio's value (`true` on Linux, macOS and Windows) since SBR wave 5
/// (lane `nioreal`), Windows since doc 56: the real `DatagramChannelImpl.innerJoin`
/// reads it, and `false` refused `new MulticastSocket().joinGroup(230.x)`.
#[test]
fn t19_5_reuse_port_and_ipv6_join_v4_are_libnio_s_answers() {
    let mut ctx = MockNativeContext::new();
    let r1 = net_is_reuse_port_available(&mut ctx, &[]).unwrap().unwrap();
    let r2 = net_can_ipv6_join_ipv4_group(&mut ctx, &[])
        .unwrap()
        .unwrap();
    let expect = i32::from(cfg!(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "windows"
    )));
    match (r1, r2) {
        (Value::Int(a), Value::Int(b)) => {
            assert_eq!(a, i32::from(cfg!(target_os = "linux")));
            assert_eq!(b, expect);
        }
        _ => panic!("expected Int results"),
    }
}

#[test]
fn io_scratch_yields_exact_len_slice() {
    // The closure must see a slice whose length matches the request exactly,
    // even though the backing Vec may carry extra capacity from a prior
    // (larger) call on the same thread.
    with_io_scratch(64, |buf| assert_eq!(buf.len(), 64));
    with_io_scratch(8, |buf| assert_eq!(buf.len(), 8));
    with_io_scratch(0, |buf| assert_eq!(buf.len(), 0));
}

#[test]
fn io_scratch_reuses_backing_without_shrinking() {
    // A large request grows the buffer; a subsequent smaller request reuses
    // the same allocation (capacity does not shrink) and still hands back a
    // correctly-sized slice. This is the property that removes the per-call
    // malloc on a steady-state socket loop.
    with_io_scratch(4096, |buf| {
        assert_eq!(buf.len(), 4096);
        buf.fill(0xAB);
    });
    let cap_after_large = IO_SCRATCH.with(|c| c.borrow().capacity());
    assert!(cap_after_large >= 4096);
    with_io_scratch(16, |buf| assert_eq!(buf.len(), 16));
    let cap_after_small = IO_SCRATCH.with(|c| c.borrow().capacity());
    assert_eq!(
        cap_after_large, cap_after_small,
        "smaller request must not shrink the reused buffer"
    );
}

#[test]
fn io_scratch_grow_zeroes_only_new_tail() {
    // Growing the buffer zero-extends the freshly added tail. Bytes the
    // caller wrote into the prefix remain (they are overwritten by the next
    // real read/write before being observed, so their staleness is benign).
    with_io_scratch(4, |buf| buf.copy_from_slice(&[1, 2, 3, 4]));
    with_io_scratch(8, |buf| {
        assert_eq!(&buf[..4], &[1, 2, 3, 4], "prefix preserved across grow");
        assert_eq!(&buf[4..], &[0, 0, 0, 0], "new tail zero-filled");
    });
}

#[test]
fn io_scratch_is_thread_local() {
    // Each thread owns an independent buffer: a write on one thread is not
    // visible to another, so concurrent read0/write0 calls cannot alias.
    with_io_scratch(4, |buf| buf.copy_from_slice(&[9, 9, 9, 9]));
    let other = thread::spawn(|| {
        with_io_scratch(4, |buf| {
            // Freshly allocated on this thread — all zero, not [9,9,9,9].
            buf.to_vec()
        })
    })
    .join()
    .unwrap();
    assert_eq!(other, vec![0, 0, 0, 0]);
}
