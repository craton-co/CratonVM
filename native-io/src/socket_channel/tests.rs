use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use std::io::{Read as _, Write as _};

#[test]
/// The wildcard is reported AS the wildcard, matching HotSpot.
///
/// This asserted the opposite until 2026-08-10 — that `0.0.0.0` became
/// `127.0.0.1` — which is why the rewrite survived: the test pinned the
/// bug. Oracle for the new expectation is Temurin 25, which answers
/// `isAnyLocalAddress() == true` from both `ServerSocketChannel
/// .getLocalAddress()` and `HttpServer.getAddress()`.
fn advertised_listener_host_reports_the_address_actually_bound() {
    assert_eq!(
        advertised_listener_host("0.0.0.0:49152".parse().unwrap()),
        "0.0.0.0"
    );
    assert_eq!(
        advertised_listener_host("[::]:49152".parse().unwrap()),
        "::"
    );
    assert_eq!(
        advertised_listener_host("127.0.0.2:49152".parse().unwrap()),
        "127.0.0.2"
    );
}

/// `RJdkNio.selectorAndAsyncClose` at the Rust layer: a reader parked on a
/// blocking channel must break out when another thread closes it.
/// `sc_close`'s `shutdown(Write)` + `tcp_remove` cannot reach a parked
/// `recv` — this reader holds an `Arc` clone of the very stream the map
/// dropped, so the OS handle stays open — which is why the wakeup has to
/// come from the registry check rather than from the socket.
#[test]
fn a_close_breaks_a_parked_blocking_channel_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Hold the accepted end open and silent so the reader genuinely parks.
    let keeper = std::thread::spawn(move || listener.accept().unwrap().0);

    let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let id = tcp_register(TcpHandle::Stream(Arc::clone(&client)));

    let reader = std::thread::spawn(move || {
        let mut buf = [0_u8; 16];
        let start = std::time::Instant::now();
        let outcome = read_close_aware(id, &client, &mut buf, &|| false);
        (outcome.map_err(|e| e.kind()), start.elapsed())
    });

    std::thread::sleep(Duration::from_millis(100));
    tcp_remove(id);

    let (outcome, elapsed) = reader.join().unwrap();
    assert_eq!(
        outcome.unwrap_err(),
        ErrorKind::Interrupted,
        "an asynchronous close must break the parked read"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "close should wake the parked reader promptly, got {elapsed:?}"
    );
    drop(keeper.join().unwrap());
}

/// The other half of the contract: a close-aware read is still a read.
/// Bytes that arrive while the reader is parked come back on the next pass,
/// not after a poll timeout.
#[test]
fn a_close_aware_read_still_delivers_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let writer = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        peer.write_all(b"ping").unwrap();
        peer
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let id = tcp_register(TcpHandle::Stream(Arc::new(client.try_clone().unwrap())));
    let mut buf = [0_u8; 16];
    let n = read_close_aware(id, &client, &mut buf, &|| false)
        .expect("read must succeed")
        .expect("a blocking close-aware read never answers UNAVAILABLE");
    assert_eq!(&buf[..n as usize], b"ping");
    tcp_remove(id);
    drop(writer.join().unwrap());
}

/// `RSocketChannelInterrupt.readWakesOnInterrupt` at the Rust layer: a
/// reader parked on a blocking channel must break out when the reading
/// THREAD is interrupted — the mechanism that is distinct from the
/// asynchronous close above, because the channel is never removed from the
/// registry by the interrupter.
///
/// The probe stands in for the parked thread's interrupt `AtomicBool`; in
/// the VM it is `NativeContext::is_interrupted(false)`, which
/// `Thread.interrupt()` sets through the thread registry.
#[test]
fn an_interrupt_breaks_a_parked_blocking_channel_read() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Held open and silent, exactly as in the async-close sibling: the
    // reader must park for want of DATA, not for want of a connection.
    let keeper = std::thread::spawn(move || listener.accept().unwrap().0);

    let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let id = tcp_register(TcpHandle::Stream(Arc::clone(&client)));
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let flag = Arc::clone(&interrupted);
    let reader = std::thread::spawn(move || {
        let mut buf = [0_u8; 16];
        let start = std::time::Instant::now();
        let outcome = read_close_aware(id, &client, &mut buf, &|| flag.load(Ordering::SeqCst));
        (outcome.map_err(|e| e.kind()), start.elapsed())
    });

    std::thread::sleep(Duration::from_millis(100));
    // NOT `tcp_remove`: the channel stays registered, so only the interrupt
    // can end this park. That is what separates this test from the
    // asynchronous-close one — if the interrupt probe were ignored, the
    // read would still be parked when the join below times the thread out.
    interrupted.store(true, Ordering::SeqCst);

    let (outcome, elapsed) = reader.join().unwrap();
    assert_eq!(
        outcome.unwrap_err(),
        ErrorKind::Interrupted,
        "an interrupt must break the parked read"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the interrupt should wake the parked reader within a poll slice, got {elapsed:?}"
    );
    assert!(
        tcp_registry().read().contains_key(&id),
        "the interrupt path must not remove the registry entry itself — \
         closing the channel is the CALLER's job, done through `sc_close` \
         on the pin-reloaded channel reference so the synthetic state \
         (`isOpen()`) is torn down with it"
    );
    tcp_remove(id);
    drop(keeper.join().unwrap());
}

/// The interrupt probe outranks a readiness edge. HotSpot does not deliver
/// buffered bytes to an interrupted blocking read (measured: the "flag set
/// before read, data ready" case still throws `ClosedByInterruptException`),
/// so a probe that only ran on the not-ready path would answer the read,
/// leave the channel open, and lose the interrupt entirely.
#[test]
fn an_interrupt_outranks_bytes_that_are_already_readable() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let writer = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        peer.write_all(b"ping").unwrap();
        peer
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let id = tcp_register(TcpHandle::Stream(Arc::new(client.try_clone().unwrap())));
    // Let the bytes actually arrive, so the poll below reports READY.
    std::thread::sleep(Duration::from_millis(200));

    let mut buf = [0_u8; 16];
    let outcome = read_close_aware(id, &client, &mut buf, &|| true);
    assert_eq!(
        outcome.map(|n| n.unwrap_or(0)).unwrap_err().kind(),
        ErrorKind::Interrupted,
        "readable bytes must not satisfy a read whose thread is interrupted"
    );
    tcp_remove(id);
    drop(writer.join().unwrap());
}

/// Regression guard for the Jetty `givenAnInflightRequestWhenTheServerIs
/// StoppedThenGracefulShutdownCallbackIsCalledWithRequestsActive` hang
/// (`jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals-FIXED.md`): `AbstractReactiveWebServerFactoryTests`
/// builds its client target via `new InetSocketAddress(port)`, which
/// produces a wildcard host. Connecting to that host verbatim throws
/// WSAEADDRNOTAVAIL on Windows instead of reaching the server, leaving
/// the test's `BlockingHandler.awaitQueue()` parked forever.
#[test]
fn connect_target_host_substitutes_loopback_for_wildcard_only() {
    assert_eq!(connect_target_host("0.0.0.0".to_string()), "127.0.0.1");
    assert_eq!(connect_target_host("::".to_string()), "::1");
    assert_eq!(connect_target_host("0:0:0:0:0:0:0:0".to_string()), "::1");
    assert_eq!(connect_target_host("127.0.0.2".to_string()), "127.0.0.2");
    assert_eq!(
        connect_target_host("example.invalid".to_string()),
        "example.invalid"
    );
}

#[test]
fn registers_without_panic() {
    let mut r = NativeMethodRegistry::new();
    register_socket_channel_real(&mut r);
    // Sanity: the most-used methods are reachable.
    assert!(r
        .find(
            "java/nio/channels/SocketChannel",
            "open",
            "()Ljava/nio/channels/SocketChannel;"
        )
        .is_some());
    assert!(r
        .find(
            "sun/nio/ch/SocketChannelImpl",
            "configureBlocking",
            "(Z)Ljava/nio/channels/SelectableChannel;"
        )
        .is_some());
    assert!(r
        .find(
            "java/nio/channels/SocketChannel",
            "bind",
            "(Ljava/net/SocketAddress;)Ljava/nio/channels/SocketChannel;"
        )
        .is_some());
    assert!(r
        .find(
            "java/nio/channels/ServerSocketChannel",
            "accept",
            "()Ljava/nio/channels/SocketChannel;"
        )
        .is_some());
}

#[test]
fn blocking_accept_observes_channel_close_promptly() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let id = tcp_register(TcpHandle::Listener(listener));

    let waiter = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let err = accept_close_aware(id, true, &|| false).unwrap_err();
        (err.kind(), start.elapsed())
    });

    std::thread::sleep(Duration::from_millis(50));
    tcp_remove(id);
    let (kind, elapsed) = waiter.join().unwrap();
    assert_eq!(kind, ErrorKind::Interrupted);
    assert!(
        elapsed < Duration::from_secs(2),
        "close-aware accept should wake promptly, got {elapsed:?}"
    );
}

/// Dropping the registry entry must close the OS socket, even with an
/// acceptor thread parked in `accept_close_aware`.
///
/// The registry is the sole owner of the listening socket, so
/// `tcp_remove` — which is what `ServerSocketChannel.close()` ends in —
/// closes the port. An acceptor that kept its own `try_clone()`d duplicate
/// silently defeated that: the port stayed open for up to one
/// `ACCEPT_CLOSE_POLL`, and a connection arriving in that window was
/// accepted and served. Spring Boot's `JettyServletWebServerFactoryTests
/// .whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` saw
/// that as `404 Not Found` where it expected a refused connection.
///
/// Assert the OS-visible property directly — a connect to the port has to
/// fail — rather than the internal one, because the internal state was
/// already correct while the socket stayed open.
#[test]
fn closing_the_registry_entry_closes_the_listening_port() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let id = tcp_register(TcpHandle::Listener(listener));

    let acceptor =
        std::thread::spawn(move || accept_close_aware(id, true, &|| false).map(|_| ()));
    // Let the acceptor reach its first poll, so the close lands with a
    // thread actively accepting.
    std::thread::sleep(Duration::from_millis(50));

    tcp_remove(id);
    assert_eq!(
        acceptor.join().unwrap().unwrap_err().kind(),
        ErrorKind::Interrupted
    );

    let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    let connected =
        std::net::TcpStream::connect_timeout(&target, Duration::from_millis(250)).is_ok();
    assert!(
        !connected,
        "port {port} still accepted a connection after its registry entry \
         was dropped — something outlived the close"
    );
}

/// The accept twin of `an_interrupt_breaks_a_parked_blocking_channel_read`:
/// an acceptor parked with the listener still REGISTERED can only be ended
/// by the interrupt probe, so if the probe were ignored the join below
/// would never return.
#[test]
fn an_interrupt_breaks_a_parked_blocking_accept() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let id = tcp_register(TcpHandle::Listener(listener));
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let flag = Arc::clone(&interrupted);
    let acceptor = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let err = accept_close_aware(id, true, &|| flag.load(Ordering::SeqCst)).unwrap_err();
        (err.kind(), start.elapsed())
    });

    std::thread::sleep(Duration::from_millis(50));
    // NOT `tcp_remove`: the listener stays registered, so only the
    // interrupt can end this park.
    interrupted.store(true, Ordering::SeqCst);

    let (kind, elapsed) = acceptor.join().unwrap();
    assert_eq!(
        kind,
        ErrorKind::Interrupted,
        "an interrupt must break the parked accept"
    );
    assert!(
        elapsed < Duration::from_secs(2),
        "the interrupt should wake the parked acceptor within a poll slice, got {elapsed:?}"
    );
    assert!(
        tcp_registry().read().contains_key(&id),
        "the interrupt path must not deregister the listener itself — closing \
         the channel is the CALLER's job, done through `sc_close` on the \
         pin-reloaded channel reference so `isOpen()` is torn down with it"
    );
    tcp_remove(id);
}

/// The interrupt probe outranks a connection already sitting in the
/// backlog, exactly as it outranks already-readable bytes on the read side.
/// Measured on HotSpot: a blocking `accept()` entered with the flag set
/// throws `ClosedByInterruptException` and does NOT hand back the pending
/// connection.
#[test]
fn an_interrupt_outranks_a_connection_already_in_the_backlog() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let id = tcp_register(TcpHandle::Listener(listener));
    // A real pending connection, so the accept below would succeed at once.
    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::thread::sleep(Duration::from_millis(100));

    let outcome = accept_close_aware(id, true, &|| true);
    assert_eq!(
        outcome.map(|p| p.is_some()).unwrap_err().kind(),
        ErrorKind::Interrupted,
        "a pending connection must not satisfy an accept whose thread is \
         interrupted"
    );
    // And the negative half: the connection is still there, so the failure
    // above was a refusal to serve it, not a lost connection.
    let served = accept_close_aware(id, true, &|| false).unwrap();
    assert!(
        served.is_some(),
        "the refused accept must have left the pending connection in the \
         backlog"
    );
    tcp_remove(id);
    drop(client);
}

/// The write twin of `a_close_breaks_a_parked_blocking_channel_read`.
///
/// Measured on this host that a raw `shutdown(SHUT_WR)` — which is all
/// `sc_close`'s `lingering_channel_close` issues — does NOT wake a writer
/// parked in a blocking-mode `send`, so the wakeup has to come from the
/// registry check, exactly as on the read side.
///
/// HotSpot returns the PARTIAL COUNT (no exception) when a close lands
/// after some bytes have gone out, which is what this asserts.
#[test]
fn a_close_breaks_a_parked_blocking_channel_write() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Accept and then never read: the peer's receive buffer and our send
    // buffer both fill, and the write parks.
    let keeper = std::thread::spawn(move || listener.accept().unwrap().0);

    let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let id = tcp_register(TcpHandle::Stream(Arc::clone(&client)));

    // Far more than any plausible socket buffer pair, so the write is
    // guaranteed to still be in progress when the close lands.
    let payload = vec![0x5a_u8; 8 * 1024 * 1024];
    let writer = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let outcome = write_close_aware(id, &client, &payload, &|| false);
        (outcome, payload.len(), start.elapsed())
    });

    std::thread::sleep(Duration::from_millis(300));
    tcp_remove(id);

    let (outcome, len, elapsed) = writer.join().unwrap();
    let n = outcome
        .expect("a close that lands after a partial transfer is not an error")
        .expect("a blocking write never answers UNAVAILABLE");
    assert!(
        n > 0 && (n as usize) < len,
        "the write must report the PARTIAL count it managed before the \
         close (got {n} of {len}); equal would mean it never parked"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "close should wake the parked writer promptly, got {elapsed:?}"
    );
    drop(keeper.join().unwrap());
}

/// The other race direction for write: the writing THREAD is interrupted
/// while parked, with the channel still registered — so only the probe can
/// end it. The registry entry must survive: closing the channel is the
/// caller's job, through `sc_close` on a pin-reloaded reference.
#[test]
fn an_interrupt_breaks_a_parked_blocking_channel_write() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let keeper = std::thread::spawn(move || listener.accept().unwrap().0);

    let client = Arc::new(TcpStream::connect(("127.0.0.1", port)).unwrap());
    let id = tcp_register(TcpHandle::Stream(Arc::clone(&client)));
    let interrupted = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let flag = Arc::clone(&interrupted);
    let payload = vec![0x5a_u8; 8 * 1024 * 1024];
    let writer = std::thread::spawn(move || {
        let start = std::time::Instant::now();
        let outcome = write_close_aware(id, &client, &payload, &|| flag.load(Ordering::SeqCst));
        (outcome.map_err(|e| e.kind()), start.elapsed())
    });

    std::thread::sleep(Duration::from_millis(300));
    interrupted.store(true, Ordering::SeqCst);

    let (outcome, elapsed) = writer.join().unwrap();
    assert_eq!(
        outcome.map(|n| n.unwrap_or(0)).unwrap_err(),
        ErrorKind::Interrupted,
        "an interrupt must break the parked write — and it outranks the \
         bytes already transferred, which is why this is an error and not \
         the partial count the close case returns"
    );
    assert!(
        elapsed < Duration::from_secs(3),
        "the interrupt should wake the parked writer within a poll slice, \
         got {elapsed:?}"
    );
    assert!(
        tcp_registry().read().contains_key(&id),
        "the interrupt path must not remove the registry entry itself"
    );
    tcp_remove(id);
    drop(keeper.join().unwrap());
}

/// The other half of the contract: a close-aware write is still a write.
/// A payload that fits must be delivered WHOLE — a blocking write may not
/// answer a short count — and the reader must see every byte in order,
/// which is the assertion that would catch a slicing bug in
/// `WRITE_SLICE_MAX`.
#[test]
fn a_close_aware_write_delivers_every_byte_in_order() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    // Larger than one slice, so the loop really does slice.
    let len = WRITE_SLICE_MAX * 5 + 17;
    let reader = std::thread::spawn(move || {
        let (mut peer, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0_u8; 64 * 1024];
        while got.len() < len {
            match peer.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) => panic!("peer read failed: {e}"),
            }
        }
        got
    });

    let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    let id = tcp_register(TcpHandle::Stream(Arc::new(client.try_clone().unwrap())));
    let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
    let n = write_close_aware(id, &client, &payload, &|| false)
        .expect("write must succeed")
        .expect("a blocking close-aware write never answers UNAVAILABLE");
    assert_eq!(n as usize, len, "a blocking write must not return short");
    let got = reader.join().unwrap();
    assert_eq!(got, payload, "sliced write must preserve content and order");
    tcp_remove(id);
}

#[test]
fn h3b_resolve_and_vet_blocks_link_local_literal() {
    // Direct link-local IP must be denied before any dial.
    crate::outbound_policy::reset_policy();
    assert!(
        resolve_and_vet("169.254.169.254:80", None).is_err(),
        "expected AWS IMDS literal to be denied"
    );
    assert!(
        resolve_and_vet("169.254.170.2:80", None).is_err(),
        "expected AWS ECS task-role-credentials address to be denied"
    );
    assert!(
        resolve_and_vet("[fd00:ec2::254]:80", None).is_err(),
        "expected IPv6 AWS metadata to be denied"
    );
}

#[test]
fn h3b_resolve_and_vet_allows_and_resolves_loopback() {
    // Loopback resolves and passes the policy; the returned addrs are
    // concrete literals (no hostname left to re-resolve downstream).
    crate::outbound_policy::reset_policy();
    let addrs = resolve_and_vet("127.0.0.1:9", None).expect("loopback should be allowed");
    assert!(!addrs.is_empty());
    assert!(addrs.iter().all(|a| a.ip().is_loopback()));
}

#[test]
fn nb_read_returns_eagain_zero() {
    // Spawn a real listener that just accepts and never sends anything.
    //
    // The accepted socket is bound to a NAMED local on purpose. `let _ =`
    // drops its value immediately, so the server end was closed the instant
    // it was accepted and the sleep below guarded nothing: the peer's FIN
    // raced this thread's `try_read_nb`, and whichever won decided the
    // result. FIN first means `Ok(0)` -> `Some(-1)`, and the assertion
    // reads `None`, so the test failed ~1 run in 12 (MEASURED over 12 runs
    // before this change: 11 pass, 1 fail). `let _named =` holds the socket
    // open for the sleep, which is what makes "no data has been sent" true
    // for the whole of the read below.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let _t = std::thread::spawn(move || {
        let _accepted = listener.accept();
        std::thread::sleep(Duration::from_secs(2));
    });
    // Connect and switch to non-blocking.
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_nonblocking(true).unwrap();
    let mut buf = [0u8; 16];
    let res = try_read_nb(&stream, &mut buf).unwrap();
    // No data has been sent — EAGAIN translates to None.
    assert!(res.is_none());
}

#[test]
fn nb_write_partial() {
    // A round-trip on a small buffer: write a few bytes and read them back.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 16];
        let n = s.read(&mut buf).unwrap();
        buf[..n].to_vec()
    });
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream.set_nonblocking(true).unwrap();
    let n = try_write_nb(&stream, b"hello").unwrap().unwrap_or(0);
    assert!(n > 0);
    // Block on the server side to confirm bytes arrived.
    let got = server.join().unwrap();
    assert_eq!(got.as_slice(), &b"hello"[..n as usize]);
}

#[test]
fn tomcat0807_http_lingering_channel_close_sends_write_fin_not_full_reset() {
    // sc_close no longer starts a background drain thread (2026-07-11 --
    // see task #11 in the swallow-uploads known-issue doc: an
    // unconditional background drain masked Tomcat's own
    // checkSwallowInput()/DISABLE_SWALLOW_INPUT intent, silently turning
    // every intentional abort-without-swallow into a graceful close).
    // What lingering_channel_close still must guarantee is the original,
    // narrower concern: a write-side FIN so a peer's blocking read sees
    // EOF instead of hanging forever (the selector-duplicate-handle
    // issue documented on sc_close itself) -- not that arbitrary-sized
    // peer uploads always complete without a reset.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        lingering_channel_close(0x0807, &stream);
        drop(stream);
    });

    let mut client = TcpStream::connect(("127.0.0.1", port)).unwrap();
    server.join().unwrap();

    // The peer's read must see a clean EOF (not hang, not error) once
    // the server's write-side FIN arrives.
    let mut buf = [0u8; 16];
    let n = client.read(&mut buf).unwrap();
    assert_eq!(n, 0, "peer read should observe EOF after write-shutdown");
}

/// gc-common w10-b (`handoff-w9b-nio-row-per-vm`): the three channel
/// side tables (`chan_fields`, `ss_back_ref_table`,
/// `ssc_socket_cache_table`) are stamped with the creating VM and walked
/// per VM. Two mocks are two VMs, at disjoint addresses (two live VMs
/// never share one, and `new()` mocks all start at 8):
///
/// * the scan for A reports only A's refs;
/// * B's remap, with a map naming A's addresses, leaves A's rows alone,
///   and A's remap rewrites them;
/// * the forget drops only A's rows.
#[test]
fn w10b_channel_rows_are_scanned_remapped_and_forgotten_per_vm() {
    use crate::test_support::MockNativeContext;
    let mut ctx_a = MockNativeContext::with_address_base(0x5B10_0000);
    let mut ctx_b = MockNativeContext::with_address_base(0x5B20_0000);
    let (vm_a, vm_b) = (ctx_a.vm_identity(), ctx_b.vm_identity());
    assert_ne!(vm_a, vm_b);
    let (chan_a, wrap_a, sock_a) = (
        ctx_a.alloc_object(0),
        ctx_a.alloc_object(0),
        ctx_a.alloc_object(0),
    );
    let (chan_b, wrap_b, sock_b) = (
        ctx_b.alloc_object(0),
        ctx_b.alloc_object(0),
        ctx_b.alloc_object(0),
    );
    // Each creation path stamps: `cf_set` (A) and `cf_opt_set` (B) both
    // create a `chan_fields` row.
    cf_set(&mut ctx_a, chan_a, F_BLOCKING, Value::Int(7));
    cf_opt_set(&ctx_b, chan_b, "SO_RCVBUF", 4096);
    ss_record_back_ref(&mut ctx_a, wrap_a, chan_a);
    ss_record_back_ref(&mut ctx_b, wrap_b, chan_b);
    ssc_socket_cache_put(&mut ctx_a, chan_a, sock_a);
    ssc_socket_cache_put(&mut ctx_b, chan_b, sock_b);

    let scan = |vm: usize| {
        let mut roots = Vec::new();
        gc_scan_channel_roots(vm, &mut roots);
        gc_scan_ss_back_ref_roots(vm, &mut roots);
        gc_scan_ssc_socket_cache_roots(vm, &mut roots);
        roots
    };
    let roots_a = scan(vm_a);
    for r in [chan_a, wrap_a, sock_a] {
        assert!(roots_a.contains(&r), "VM A's scan must root its own {r:?}");
    }
    for r in [chan_b, wrap_b, sock_b] {
        assert!(
            !roots_a.contains(&r),
            "VM A's scan must not root VM B's {r:?}"
        );
    }
    let roots_b = scan(vm_b);
    assert!(
        roots_b.contains(&chan_b) && roots_b.contains(&wrap_b) && roots_b.contains(&sock_b)
    );
    assert!(!roots_b.contains(&chan_a));

    let moved = |r: ObjectRef| {
        // SAFETY: non-null and 8-aligned (an 8-aligned mock address plus
        // an 8-aligned offset); compared by identity only, never read.
        unsafe { ObjectRef::from_raw((r.as_ptr() as usize + 0x0100_0000) as *mut u8) }
    };
    let mut map = cratonvm_types::PointerMap::default();
    for r in [chan_a, wrap_a, sock_a] {
        map.insert(r.as_ptr() as usize, moved(r).as_ptr() as usize);
    }
    let remap = |vm: usize, map: &cratonvm_types::PointerMap| {
        channel_fields_update_after_gc(vm, map);
        ss_back_ref_update_after_gc(vm, map);
        ssc_socket_cache_update_after_gc(vm, map);
    };

    // B's remap leaves A's rows where they are: A's lookups still hit.
    remap(vm_b, &map);
    assert_eq!(cf_get(&ctx_a, chan_a, F_BLOCKING), Value::Int(7));
    assert_eq!(ss_back_ref(&mut ctx_a, wrap_a), Some(chan_a));
    assert_eq!(ssc_socket_cache_get(&mut ctx_a, chan_a), Some(sock_a));

    // A's remap rewrites them (the identity-hash bucket stays put).
    remap(vm_a, &map);
    let (h_chan, h_wrap) = (
        ctx_a.identity_hash_code(chan_a),
        ctx_a.identity_hash_code(wrap_a),
    );
    assert!(chan_fields().read()[&h_chan]
        .iter()
        .any(|s| s.vm == vm_a && s.object == moved(chan_a)));
    assert!(ss_back_ref_table().read()[&h_wrap]
        .iter()
        .any(|r| r.vm == vm_a && r.wrapper == moved(wrap_a) && r.channel == moved(chan_a)));
    assert!(ssc_socket_cache_table().read()[&h_chan]
        .iter()
        .any(|r| r.vm == vm_a && r.channel == moved(chan_a) && r.socket == moved(sock_a)));
    assert_eq!(cf_opt_get(&ctx_b, chan_b, "SO_RCVBUF"), Some(4096));
    assert_eq!(ss_back_ref(&mut ctx_b, wrap_b), Some(chan_b));
    assert_eq!(ssc_socket_cache_get(&mut ctx_b, chan_b), Some(sock_b));

    // Forgetting A drops A's rows only.
    forget_vm_nio_rows(vm_a);
    assert!(scan(vm_a).is_empty(), "VM A's rows must be gone");
    assert!(!chan_fields()
        .read()
        .get(&h_chan)
        .is_some_and(|b| b.iter().any(|s| s.vm == vm_a)));
    assert_eq!(cf_opt_get(&ctx_b, chan_b, "SO_RCVBUF"), Some(4096));
    assert_eq!(ss_back_ref(&mut ctx_b, wrap_b), Some(chan_b));
    assert_eq!(ssc_socket_cache_get(&mut ctx_b, chan_b), Some(sock_b));
    // Idempotent.
    forget_vm_nio_rows(vm_a);

    forget_vm_nio_rows(vm_b);
    assert!(scan(vm_b).is_empty());
    assert_eq!(cf_opt_get(&ctx_b, chan_b, "SO_RCVBUF"), None);
}

/// Round 15 wave 1 (lane shadow): the JDK 25 `setOption` / `getOption`
/// checks, in the JDK's order (NPE, UOE, IAE, closed), and the two
/// narrowings (a non-standard option is never UOE; a raw `Int` value is
/// never IAE). Page: r15w1-shadow-socket-channel-set-option-skips-checks.
#[test]
fn nio_option_checks_follow_the_jdk_order() {
    use crate::test_support::MockNativeContext;
    use cratonvm_types::error::VmError;
    fn runtime(r: Result<(), MethodCallFailed>) -> Option<RuntimeError> {
        match r {
            Err(MethodCallFailed::InternalError(VmError::Runtime(e))) => Some(e),
            _ => None,
        }
    }
    let mut ctx = MockNativeContext::with_address_base(0x5B30_0000);
    let chan = ctx.alloc_object_with_class(0, "sun/nio/ch/SocketChannelImpl");
    let server = ctx.alloc_object_with_class(0, "sun/nio/ch/ServerSocketChannelImpl");
    let std_opt =
        ctx.alloc_object_with_class(0, "java/net/StandardSocketOptions$StdSocketOption");
    let ext_opt =
        ctx.alloc_object_with_class(0, "jdk/net/ExtendedSocketOptions$ExtSocketOption");
    let boolean = ctx.alloc_object_with_class(0, "java/lang/Boolean");
    let string = ctx.alloc_object_with_class(0, "java/lang/String");

    // Null option: NPE with no message, before anything else is asked.
    assert!(matches!(
        runtime(nio_option_checks(&mut ctx, chan, None, "", None)),
        Some(RuntimeError::NullPointerException { message: None })
    ));
    // A standard option the channel kind does not support: UOE.
    match runtime(nio_option_checks(
        &mut ctx,
        chan,
        Some(std_opt),
        "IP_MULTICAST_TTL",
        None,
    )) {
        Some(RuntimeError::UnsupportedOperationException { message }) => {
            assert_eq!(message, "'IP_MULTICAST_TTL' not supported");
        }
        other => panic!("expected UOE, got {other:?}"),
    }
    assert!(matches!(
        runtime(nio_option_checks(
            &mut ctx,
            server,
            Some(std_opt),
            "TCP_NODELAY",
            None
        )),
        Some(RuntimeError::UnsupportedOperationException { .. })
    ));
    // Wrong-typed and null values: IAE.
    assert!(matches!(
        runtime(nio_option_checks(
            &mut ctx,
            chan,
            Some(std_opt),
            "TCP_NODELAY",
            Some(Value::Object(Some(string))),
        )),
        Some(RuntimeError::IllegalArgumentException { .. })
    ));
    match runtime(nio_option_checks(
        &mut ctx,
        chan,
        Some(std_opt),
        "TCP_NODELAY",
        Some(Value::Object(None)),
    )) {
        Some(RuntimeError::IllegalArgumentException { message }) => {
            assert_eq!(message, "Invalid value 'null'");
        }
        other => panic!("expected IAE, got {other:?}"),
    }
    // Well-typed on a channel with no open record: closed, not Ok.
    assert!(nio_option_checks(
        &mut ctx,
        chan,
        Some(std_opt),
        "TCP_NODELAY",
        Some(Value::Object(Some(boolean))),
    )
    .is_err());
    cf_set(&mut ctx, chan, F_OPEN, Value::Int(1));
    assert!(nio_option_checks(
        &mut ctx,
        chan,
        Some(std_opt),
        "TCP_NODELAY",
        Some(Value::Object(Some(boolean))),
    )
    .is_ok());
    // Narrowings: an extended option is never UOE, a raw Int never IAE.
    assert!(nio_option_checks(&mut ctx, chan, Some(ext_opt), "TCP_KEEPIDLE", None).is_ok());
    assert!(nio_option_checks(
        &mut ctx,
        chan,
        Some(std_opt),
        "TCP_NODELAY",
        Some(Value::Int(1)),
    )
    .is_ok());
    forget_vm_nio_rows(ctx.vm_identity());
}
