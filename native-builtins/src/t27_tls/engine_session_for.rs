#[allow(unused_imports)]
use super::*;

/// `getSession()`'s stable answer: the cached session for this engine's current
/// handshake epoch, built on first use. See [`engine_session_table`].
pub fn engine_session_for(
    ctx: &mut dyn NativeContext,
    engine: ObjectRef,
    id: i32,
    door: SessionDoor,
) -> Result<ObjectRef, MethodCallFailed> {
    let handshaked = with_engine(id, |s| {
        s.conn
            .as_ref()
            .map(|c| !c.is_handshaking())
            .unwrap_or(false)
    })
    .unwrap_or(false);
    let phase = session_phase(door, handshaked);
    let key = (engine_objref_key(ctx, engine), phase);
    if let Some(existing) = engine_session_table().lock().get(&key).copied() {
        return Ok(existing);
    }
    // A RESUMED connection continues the previous session, so it must answer
    // with the previous session OBJECT — its `putValue` bindings and its
    // creation time are what "resumed" means to an application. See
    // `client_session_cache`.
    let cache_key = if phase == SessionPhase::Negotiated {
        client_session_cache_key(id)
    } else {
        None
    };
    if let Some(ck) = cache_key.as_ref() {
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] engine_session_for id={} key={:?} resumed={} kind={:?} cached={}",
                id,
                ck,
                engine_handshake_was_resumed(id),
                with_engine(id, |s| s.conn.as_ref().and_then(|c| c.handshake_kind())).flatten(),
                client_session_cache().lock().contains_key(ck)
            );
        }
        if engine_handshake_was_resumed(id) {
            // Bound to a local first: as an `if let` scrutinee (edition 2021)
            // the guard would live for the whole body, which takes
            // `engine_session_table` and re-enters the VM through
            // `touch_session_access_time`. `ObjectRef` is `Copy`, so the read
            // is complete once the guard drops.
            let prev = client_session_cache().lock().get(ck).copied();
            if let Some(prev) = prev {
                engine_session_table().lock().insert(key, prev);
                touch_session_access_time(ctx, prev);
                return Ok(prev);
            }
        }
    }
    let ses = build_synthetic_ssl_session(ctx, id)?;
    // What `ExtendedSSLSession.getRequestedServerNames()` / `getPeerSupportedSignatureAlgorithms()` answer:
    // the `server_name` this connection carries and which end this is. The null session (nothing requested
    // yet) records nothing and so answers HotSpot's empty list. A CLIENT carries the name it was created
    // for when JSSE would send it (a dotless host such as `localhost` is not); a SERVER the one it peeked
    // out of the ClientHello, kept in `requested_sni` because the rustls connection can be on loan to the
    // record loop while a `TrustManager` asks.
    if phase != SessionPhase::Fresh {
        let (sni, server_side) = with_engine(id, |s| {
            let sni = if s.is_client {
                s.peer_host.clone().filter(|h| jsse_would_send_sni(h))
            } else {
                s.requested_sni.clone().or_else(|| match &s.conn {
                    Some(EngineConn::Server(c)) => c.server_name().map(str::to_string),
                    _ => None,
                })
            };
            (sni, !s.is_client)
        })
        .unwrap_or((None, false));
        record_session_sni(&*ctx, ses, sni, server_side);
    }
    if phase == SessionPhase::Negotiated {
        // Carry the BINDINGS of the session that was being negotiated — not
        // the object, whose identity must not change, and NEVER the `Fresh`
        // one. JSSE has `getHandshakeSession()` and the later `getSession()`
        // as one session, so a mid-handshake `putValue` is still bound
        // afterwards: `SSLEngineTest.mustCallResumeTrustedOnSessionResumption`
        // writes through the first and blocks reading the second, and HUNG
        // when the write landed on an object the read never saw. The null
        // session's bindings, by contrast, must NOT carry — which is the other
        // half `testSessionAfterHandshake` asserts.
        let pending = engine_session_table()
            .lock()
            .get(&(key.0, SessionPhase::Handshaking))
            .copied();
        if let Some(pending) = pending {
            let attrs_slot = session_shape_width(&*ctx, pending) - 1;
            if let Value::Object(Some(attrs)) = sess_get(&*ctx, pending, attrs_slot) {
                // Only when the pending session actually has a map — allocating
                // one here would hand every negotiated session a non-null slot
                // it did not have before.
                let ses_slot = session_shape_width(&*ctx, ses) - 1;
                sess_set(&*ctx, ses, ses_slot, Value::Object(Some(attrs)));
            }
        }
        let k = gc_stable_objref_key(ctx, ses);
        negotiated_session_keys().lock().insert(k);
        // NOM-1 (G7-1). Same epoch as the line above: this is a COMPLETED
        // handshake, so `getSessionContext()` will hand out a real carrier
        // for `ses` from this point on, and that carrier's `getIds()`/
        // `getSession(id)` need `ses` to actually be findable.
        let id_bytes = session_id_bytes(ctx, ses);
        let vm = ctx.vm_identity();
        // Bounded like JSSE's cache (gc-common w12-a).
        let _evicted =
            session_id_registry_insert_bounded(vm, id_bytes, ses, SESSION_ID_REGISTRY_PER_VM);
    }
    engine_session_table().lock().insert(key, ses);
    if let Some(ck) = cache_key {
        client_session_cache().lock().insert(ck, ses);
    }
    Ok(ses)
}

/// Build a synthetic `SSLSession` reflecting `id`'s negotiated cipher/protocol/
/// ALPN state, or — before/outside a handshake — JSSE's own "nothing was
/// negotiated" answers. Shared by `getSession()` and `getHandshakeSession()`
/// — see the latter's registration for why real JDK's `getHandshakeSession()`
/// cannot be left un-intercepted on this engine implementation.
pub fn build_synthetic_ssl_session(
    ctx: &mut dyn NativeContext,
    id: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    let (proto, cipher, alpn) = with_engine(id, |s| {
        let proto = match s.conn.as_ref().and_then(|c| c.protocol_version()) {
            Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
            Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
            // E12: no connection means no negotiated version. `"TLSv1.3"`
            // here reported the VM's DEFAULT as though it were the outcome.
            _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
        };
        // E12: JSSE's own answer for "no cipher negotiated", NOT a guess. The
        // literal that used to stand here (`TLS_AES_256_GCM_SHA384`) is in
        // HotSpot's SUPPORTED suite list and is byte-for-byte what a real TLS
        // 1.3 handshake produces, so no test a caller can write separated this
        // fallback from a genuine negotiation — and security-sensitive code
        // branches on this string. The sentinel is UNOFFERABLE
        // (`setEnabledCipherSuites("SSL_NULL_WITH_NULL_NULL")` throws
        // IllegalArgumentException, measured), so it can never be confused
        // with a negotiation. See `JSSE_NULL_CIPHER_SUITE` and
        // docs/feature-designs/native-dispatch-architecture.md
        // E12-1-the-null-session-and-the-fabricated-cipher.md §1-§2.
        let cipher = s
            .conn
            .as_ref()
            .and_then(|c| c.negotiated_cipher_suite())
            .map(|cs| suite_to_java_cipher_name(cs.suite()))
            .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into());
        let alpn = s.negotiated_alpn.clone().unwrap_or_default();
        (proto.to_string(), cipher, alpn)
    })
    .unwrap_or_else(|| {
        (
            crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL.into(),
            crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into(),
            String::new(),
        )
    });
    // E12: slot 2 is the `isValid` flag, and this constructor also serves
    // engines that have NEVER handshaked. HotSpot answers `isValid() == false`
    // for a session with no negotiation (measured, E12-1 §1) — writing 1
    // unconditionally was a third fabrication beside the cipher and the id.
    // A session is valid iff a connection actually negotiated something.
    //
    // ORDERING: this must never land ahead of the cipher/protocol sentinels
    // above. On its own it would make `isValid()` false while
    // `getCipherSuite()` still answered `TLS_AES_256_GCM_SHA384` — a session
    // reporting a strong suite while denying it is valid, a state no real
    // JSSE session can be in and more confusing than either bug alone.
    //
    // NOT tied to `invalidate()`: measured (E12-1 §1 arm E), `invalidate()`
    // changes `isValid()` and nothing else — cipher, protocol and id all
    // survive it — and closing the socket does not invalidate at all (arm G).
    let negotiated = with_engine(id, |s| {
        s.conn
            .as_ref()
            .and_then(|c| c.negotiated_cipher_suite())
            .is_some()
    })
    .unwrap_or(false);
    // 8-field synthetic session: cipher, protocol, valid, peerHost, peerPort,
    // creationTime, alpn, attrs (slot 7 — see `sslsess_attrs_slot`).
    let ses = alloc_session_carrier(ctx, 8)?;
    // A new object: no row a collected session left under its key survives
    // (gc-common w11-a, see `forget_session_rows`).
    forget_session_rows(ctx, ses);
    // (What `getRequestedServerNames()` answers is recorded by `engine_session_for`, which knows the
    // phase; this constructor also builds the null session.)
    // GC (found while landing E12): each `create_string` allocates and is
    // therefore a GC point, so a moving young collection between the
    // allocation above and the `set_field`s below relocates `ses` and every
    // write lands through a stale reference. `phases_late::ssl_security
    // ::new13_alloc_null_ssl_session` pins for exactly this reason; this
    // constructor is the one that needed it most, because it runs on EVERY
    // `getSession()`/`getHandshakeSession()` call at arbitrary allocation
    // pressure rather than once at socket construction.
    let pin = ctx.pin_native_root(ses);
    let cipher_s = ctx.create_string(&cipher);
    let cipher_pin = ctx.pin_native_root(cipher_s);
    let proto_s = ctx.create_string(&proto);
    let proto_pin = ctx.pin_native_root(proto_s);
    let alpn_s = ctx.create_string(&alpn);
    let alpn_pin = ctx.pin_native_root(alpn_s);
    // The host and port a CLIENT engine was created for (`createSSLEngine(host, port)`): HotSpot answers
    // them from `getPeerHost()` / `getPeerPort()` of the completed session; null / -1 before one, and on
    // a server engine, which was never told.
    let peer = if negotiated {
        with_engine(id, |s| {
            if s.is_client {
                s.peer_host.clone().map(|h| (h, s.peer_port))
            } else {
                None
            }
        })
        .flatten()
    } else {
        None
    };
    let peer_s = peer.as_ref().map(|(h, _)| ctx.create_string_uninterned(h));
    let ses = ctx.read_native_pin(pin, ses);
    let cipher_s = ctx.read_native_pin(cipher_pin, cipher_s);
    let proto_s = ctx.read_native_pin(proto_pin, proto_s);
    let alpn_s = ctx.read_native_pin(alpn_pin, alpn_s);
    sess_set(&*ctx, ses, 0, Value::Object(Some(cipher_s)));
    sess_set(&*ctx, ses, 1, Value::Object(Some(proto_s)));
    sess_set(&*ctx, ses, 2, Value::Int(if negotiated { 1 } else { 0 }));
    match (peer_s, peer.as_ref()) {
        (Some(h), Some((_, port))) => {
            sess_set(&*ctx, ses, 3, Value::Object(Some(h)));
            sess_set(&*ctx, ses, 4, Value::Int(*port));
        }
        _ => {
            sess_set(&*ctx, ses, 3, Value::Object(None));
            sess_set(&*ctx, ses, 4, Value::Int(-1));
        }
    }
    sess_set(&*ctx, ses, 5, Value::Long(crate::epoch_millis_now()));
    sess_set(&*ctx, ses, 6, Value::Object(Some(alpn_s)));
    // The real, on-the-wire session id, for the one case where it exists and
    // both ends must agree: a completed TLS 1.2 handshake. Under TLS 1.3 the
    // field is a meaningless echo and `getId()`'s per-object pseudo-id is the
    // right answer (see `peek_server_hello_session_id`).
    if proto == "TLSv1.2" {
        let sid = with_engine(id, |s| s.negotiated_session_id.clone()).unwrap_or_default();
        if !sid.is_empty() {
            // Key computed before the guard: `gc_stable_objref_key` calls
            // `ctx.identity_hash_code`, and this table's `LockLevel` claims it
            // is never held across a re-entry into the VM.
            let wire_key = gc_stable_objref_key(ctx, ses);
            session_wire_id_table().lock().insert(wire_key, sid);
        }
    }
    // GC: the pin taken around the `create_string`/`set_field` pairs above is
    // released HERE rather than immediately after them. Nothing between the two
    // points allocates, and holding it across the wire-id insert keeps `ses`
    // stable for that block's `gc_stable_objref_key`.
    ctx.unpin_native_roots(pin);
    // Associate the peer (client) cert chain with this session object so
    // SSLSession.getPeerCertificates() can return it for mTLS auth.
    let peer_chain = with_engine(id, |s| s.peer_cert_chain_der.clone()).unwrap_or_default();
    if !peer_chain.is_empty() {
        // Key before the guard (gc-common w26-b): it takes the lock-key
        // registry, which a sweep takes too.
        let key = gc_stable_objref_key(ctx, ses);
        session_peer_certs_table().lock().insert(key, peer_chain);
    }
    // Associate THIS side's own (local) cert chain so
    // SSLSession.getLocalCertificates() can return it. Required by Jetty's
    // SecureRequestCustomizer.getX509() (called from retrieveSni/checkSni on
    // every HTTPS request): it calls getLocalCertificates() and, on an empty
    // result, throws `HttpException.RuntimeException(400, "Invalid SNI")`
    // unconditionally — a server ALWAYS presents a certificate, so an empty
    // chain here failed every HTTPS request through a `SecureRequestCustomizer`
    // (JettyServletWebServerFactoryTests/JettyReactiveWebServerFactoryTests).
    //
    // A CLIENT, though, reports what it SENT, not what it had available: JSSE
    // answers null on a client whose `KeyManager` was configured but never
    // consulted, because the server ran `ClientAuth.NONE` and asked for
    // nothing. `RecordingClientCertResolver` is the signal (rustls exposes
    // none of its own); `None` — no resolver installed, i.e. no client identity
    // at all — is also "nothing sent". A SERVER always presents its
    // certificate, so its branch is unconditional.
    let vm = ctx.vm_identity();
    let local_chain_pem = with_engine(id, |s| {
        if s.is_client
            && !s
                .client_cert_presented
                .as_ref()
                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(false)
        {
            return None;
        }
        s.identity_override
            .as_ref()
            .map(|(cert, _)| cert.clone())
            .or_else(|| {
                (!s.is_client)
                    .then(|| runtime_tls_identity(vm))
                    .flatten()
                    .map(|rti| rti.cert_pem)
            })
    })
    .flatten();
    let local_chain: Vec<Vec<u8>> = local_chain_pem
        .and_then(|pem| parse_cert_chain_pem(&pem).ok())
        .map(|certs| certs.iter().map(|c| c.as_ref().to_vec()).collect())
        .unwrap_or_default();
    // G51: was an open-coded `.lock().insert(..)`, the table's only writer.
    // Routed through `record_local_cert_chain` so the accept path added there
    // and this one cannot drift — including the empty-chain contract, which is
    // load-bearing (`client.localCertificates = null` is a measured green row).
    record_local_cert_chain(ctx, ses, local_chain);
    Ok(ses)
}

/// `getPeerHost()` as `String.valueOf` would render it — including the literal
/// `null` HotSpot prints for an engine built by the no-arg `createSSLEngine()`.
pub fn engine_peer_host_text(ctx: &mut dyn NativeContext, engine: ObjectRef) -> String {
    match ctx.invoke_virtual(engine, "getPeerHost", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_else(|| "null".into()),
        _ => "null".to_string(),
    }
}

/// `getPeerPort()`. A CratonVM engine that never had a host recorded reads the
/// field, which `createSSLEngine()` seeds to -1 — the value
/// `javax.net.ssl.SSLEngine`'s own field initialiser uses, and the one HotSpot
/// prints.
pub fn engine_peer_port_text(ctx: &mut dyn NativeContext, engine: ObjectRef) -> String {
    match ctx.invoke_virtual(engine, "getPeerPort", "()I", &[]) {
        Ok(Some(v)) => v.as_int().unwrap_or(-1).to_string(),
        _ => "-1".to_string(),
    }
}

/// The session, rendered by its own `toString()`.
///
/// Deliberately a virtual call and not a local format: the `Session(...)` shape
/// is JSSE's `SSLSessionImpl.toString()` and belongs in one place. A session
/// this engine cannot produce renders as `null`, which is what string
/// concatenation of a null reference does.
pub fn engine_session_text(ctx: &mut dyn NativeContext, engine: ObjectRef) -> String {
    let session: ObjectRef =
        match ctx.invoke_virtual(engine, "getSession", "()Ljavax/net/ssl/SSLSession;", &[]) {
            Ok(Some(Value::Object(Some(sess)))) => sess,
            _ => return "null".to_string(),
        };
    match ctx.invoke_virtual(session, "toString", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_else(|| "null".into()),
        _ => "null".to_string(),
    }
}

pub fn register_engine_impl_natives(r: &mut NativeMethodRegistry) {
    let cls_impl = "sun/security/ssl/SSLEngineImpl";

    // toString() — NOT one of the methods this file used to register, so the
    // real JDK body ran, and JDK 25's is
    //
    //     "SSLEngine[hostname=" + getPeerHost() + ", port=" + getPeerPort()
    //             + ", " + conContext.conSession + "]"
    //
    // `conContext` is a `TransportContext` that only SunJSSE's own constructor
    // chain creates, and CratonVM never runs it. So `String.valueOf(engine)` —
    // any log line, assertion message or `IllegalStateException("… " + engine)`
    // that mentions an engine — threw
    // `NullPointerException: Cannot read field "conSession" because
    // "this.conContext" is null`, from inside the failure path rather than
    // from the code under test. Measured against HotSpot 25 with
    // `probes/SslContextSpiProbe.java`.
    //
    // Rebuilt from the engine's OWN accessors. `getPeerHost`/`getPeerPort` have
    // no native here, so they run the JDK's field reads — and those fields are
    // real: `set_engine_peer_host` writes `peerHost`/`peerPort` as well as the
    // side table, and `createSSLEngine()`'s no-arg form seeds `peerPort = -1`
    // exactly as `javax.net.ssl.SSLEngine`'s own field initialiser does. The
    // session half goes through `getSession().toString()` rather than being
    // formatted here, so the `Session(...)` shape lives in ONE place (the
    // `javax/net/ssl/SSLSession` registration in `register_ssl_session_real`)
    // and `String.valueOf(session)` is right on its own too.
    r.register(cls_impl, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = format!(
            "SSLEngine[hostname={}, port={}, {}]",
            engine_peer_host_text(ctx, this),
            engine_peer_port_text(ctx, this),
            engine_session_text(ctx, this),
        );
        let s = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(s))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // Constructor — allocates an engine_id slot in the side-table.
    r.register(cls_impl, "<init>", "()V", |ctx, args| {
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let _ = engine_id_or_alloc(ctx, *this);        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // setUseClientMode(Z)V — this pair is the LIVE one. The engine
    // `SSLContext.createSSLEngine()` hands out is a
    // `sun.security.ssl.SSLEngineImpl`, so these, registered on the concrete
    // class, beat the two copies on `javax/net/ssl/SSLEngine` in
    // `phases_late::ssl_security` and `tls.rs` (all three now agree; a
    // reordering must not resurrect a different answer).
    r.register(cls_impl, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        set_engine_use_client_mode(ctx, this, mode != 0);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // The getter reports what was CONFIGURED, which for a fresh engine is
    // nothing — HotSpot answers `false` there, and this answered `true`
    // because it read the handshake default. See
    // `EngineState::client_mode_set`: the two are deliberately separate, so
    // the report is honest without moving the default every client relies on.
    // MEASURED, `L6TlsParamSweep` rows 66 and 89.
    r.register(cls_impl, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(engine_use_client_mode(
            ctx, this,
        )))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // WAS A KNOWN DIVERGENCE (waves 2-4): this pair was inert — the selector
    // was accepted, never invoked, and the getter answered null so nothing
    // could observe the loss. The escalation the old note described (let the
    // ClientHello's offered list reach a Java `BiFunction` before rustls picks)
    // is what `engine_apply_alpn_selector` now does, by parsing the hello in
    // `do_unwrap` ahead of `engine_begin` — no cross-module hook needed, and
    // no `Acceptor`.
    //
    // Why it matters more than "custom selection policy": netty's
    // `JdkAlpnSslEngine` configures a CLIENT engine through
    // `SSLParameters.setApplicationProtocols` but a SERVER engine ONLY through
    // this setter. With the setter inert a server advertised no protocols at
    // all, so ALPN negotiated to null on both sides of every JDK-provider
    // connection.
    r.register(
        cls_impl,
        "setHandshakeApplicationProtocolSelector",
        "(Ljava/util/function/BiFunction;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key = engine_objref_key(ctx, this);
            // gc-common w12-a: the engine owns this row (swept with it).
            note_tls_weak_owner(ctx, TlsOwnerKind::Engine, key, this);
            match args.get(1) {
                Some(Value::Object(Some(f))) => {
                    engine_alpn_selector_table().lock().insert(key, *f);
                    // A NEW selector is a root again until it is consumed.
                    alpn_selector_unrooted().lock().remove(&key);
                }
                // An explicit null CLEARS the selector, per `SSLEngine`'s
                // contract ("null to remove").
                _ => {
                    engine_alpn_selector_table().lock().remove(&key);
                    alpn_selector_unrooted().lock().remove(&key);                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Now a truthful echo of what the setter stored: netty reads this pair to
    // decide whether JDK-style ALPN callbacks are live, and they are.
    r.register(
        cls_impl,
        "getHandshakeApplicationProtocolSelector",
        "()Ljava/util/function/BiFunction;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // A lookup: an engine that was never keyed has no selector
            // (gc-common w26-b).
            let key = match existing_objref_key(ctx, this) {
                Some(key) => key,
                None => return Ok(Some(Value::Object(None))),
            };
            let f = engine_alpn_selector_table().lock().get(&key).copied();
            // gc-common w12-a: an UNROOTED selector is a weak read, like
            // `Reference.get()`: keep it alive for a concurrent mark in
            // progress before handing it to the mutator.
            if let Some(f) = f {
                if !alpn_selector_is_rooted(key) {
                    ctx.gc_reference_keep_alive(f);
                }
            }
            Ok(Some(Value::Object(f)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(cls_impl, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|x| x.as_int()).unwrap_or(0) != 0;
        let id = engine_id_or_alloc(ctx, this);
        if crate::nbflags().dbg_tls_auth_ok {
            let conn_is_some = with_engine(id, |s| s.conn.is_some()).unwrap_or(false);
            eprintln!(
                "[dbg-tls-auth] DIRECT setNeedClientAuth id={} v={} conn_already_realized={}",
                id, v, conn_is_some
            );
        }
        with_engine(id, |s| {
            s.need_client_auth = v;
            if v {
                s.want_client_auth = false;            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls_impl, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let v = with_engine(id, |s| s.need_client_auth).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls_impl, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|x| x.as_int()).unwrap_or(0) != 0;
        let id = engine_id_or_alloc(ctx, this);
        with_engine(id, |s| {
            s.want_client_auth = v;
            if v {
                s.need_client_auth = false;            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(cls_impl, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let v = with_engine(id, |s| s.want_client_auth).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);

    // setEnabledProtocols / getEnabledProtocols
    r.register(
        cls_impl,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let mut list: Vec<String> = Vec::new();
            let mut given = 0usize;
            if let Some(Value::Object(Some(arr))) = args.get(1) {
                given = ctx.array_length(*arr);
                for i in 0..given {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        if let Some(t) = ctx.read_string(s) {
                            list.push(t);
                        }
                    }
                }
            }
            // An EXPLICITLY empty array means "nothing enabled", and JSSE keeps
            // it: `SSLEngineImpl.setEnabledProtocols` stores
            // `ProtocolVersion.namesOf(protocols)` verbatim and only rejects
            // null, so the next `getEnabledProtocols()` answers an empty array
            // and a handshake attempt fails with "no appropriate protocol".
            // Substituting the defaults told the caller its disable had been
            // ignored — netty's
            // `SSLEngineTest.testEnablingAnAlreadyDisabledSslProtocol` asserts
            // exactly that round trip (`array lengths differ, expected: <0> but
            // was: <2>`).
            //
            // The defaulting stays for the OTHER way `list` can end up empty —
            // a non-empty array whose entries this native could not read back —
            // where falling back to a negotiable pair is a safety net rather
            // than a contradiction of the caller.
            if list.is_empty() && given > 0 {
                list = vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()];
            }
            with_engine(id, |s| {
                s.enabled_protocols = list;            });
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls_impl,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let list = with_engine(id, |s| s.enabled_protocols.clone())
                .unwrap_or_else(|| vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]);
            // Typed and rooted — see `materialize_java_string_array`.
            let arr = materialize_java_string_array(ctx, &list);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls_impl,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Single source of truth — see `SUPPORTED_PROTOCOL_NAMES`, which
            // carries the TLSv1.1 rationale this copy used to state alone.
            let protos = SUPPORTED_PROTOCOL_NAMES;
            let arr = materialize_java_string_array(ctx, protos);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getSupportedCipherSuites() — the connector validates its configured cipher
    // list against this; without it the engine reported zero supported ciphers
    // and Tomcat threw "None of the [ciphers] specified are supported by the SSL
    // engine". Return the rustls-negotiable suites (overlaps Tomcat's defaults).
    r.register(
        cls_impl,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Single source of truth — see `SUPPORTED_CIPHER_SUITE_NAMES`.
            let suites = SUPPORTED_CIPHER_SUITE_NAMES;
            let arr = materialize_java_string_array(ctx, suites);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls_impl,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let mut list: Vec<String> = Vec::new();
            if let Some(Value::Object(Some(arr))) = args.get(1) {
                let len = ctx.array_length(*arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        if let Some(t) = ctx.read_string(s) {
                            list.push(t);
                        }
                    }
                }
            }
            // JSSE VALIDATES the names: `CipherSuite.validValuesOf` throws
            // `IllegalArgumentException("Unsupported CipherSuite: <name>")` for
            // anything that is not a cipher-suite name, and callers rely on
            // that both to fail loudly and as a probe — netty's
            // `JdkSslContext.supportedCiphers` catches the IAE to decide
            // whether an `SSL_`-prefixed alias exists, and
            // `SslContextBuilderTest.testInvalidCipherJdk` /
            // `SSLEngineTest.testInvalidCipher` assert it for a made-up name.
            // Storing whatever arrived meant a typo'd or bogus cipher list was
            // accepted here and then quietly widened by `cipher_provider_for`
            // (which falls back to the full provider when nothing maps), so
            // the application ran unrestricted believing it had restricted.
            //
            // The accepted set is deliberately broader than what this engine
            // can NEGOTIATE: JSSE accepts every name in its cipher-suite
            // registry, including suites that are supported-but-disabled, and
            // rejecting those here would break a caller that legitimately
            // names one. A name is taken as a cipher-suite name when it maps to
            // a rustls suite, is one we advertise, or carries the `TLS_`/`SSL_`
            // prefix every JSSE suite name has.
            if let Some(bad) = list.iter().find(|n| !is_cipher_suite_name(n.as_str())) {
                let msg = format!("Unsupported CipherSuite: {bad}");
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/lang/IllegalArgumentException",
                    &msg,
                ));
            }
            with_engine(id, |s| {
                s.enabled_ciphers = list;            });
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls_impl,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let list = with_engine(id, |s| s.enabled_ciphers.clone()).unwrap_or_default();
            // E42, again, one class over: the default was THREE hard-coded
            // TLS 1.3 names while this same registrar's
            // `getSupportedCipherSuites` twenty lines up answers all fifteen
            // of `SUPPORTED_CIPHER_SUITE_NAMES`. HotSpot has no
            // enabled/supported distinction on a fresh engine (measured:
            // 31 == 31, element-wise), so a caller intersecting its own list
            // with `getEnabledCipherSuites()` — netty's `JdkSslContext`
            // does exactly that — silently lost every TLS 1.2 suite this VM
            // can actually negotiate.
            let names: Vec<String> = if list.is_empty() {
                SUPPORTED_CIPHER_SUITE_NAMES
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                list
            };
            let arr = materialize_java_string_array(ctx, &names);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // beginHandshake — realize the rustls connection.
    r.register(cls_impl, "beginHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        // Real `SSLEngineImpl.beginHandshake()` refuses on a closed engine:
        // `TransportContext.kickstart` throws `SSLException("Engine is
        // closing/closed")` once either direction has been shut down.
        // `SSLEngineTest.testBeginHandshakeAfterEngineClosed` asserts exactly
        // that (closeInbound + closeOutbound, then `beginHandshake()` must
        // throw); returning normally left the test at a bare `fail()`.
        let closed = with_engine(id, |s| s.closed_inbound || s.closed_outbound).unwrap_or(false);
        if closed {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLException",
                "Engine is closing/closed",
            ));
        }
        // A SERVER engine with a handshake ALPN selector installed must NOT
        // realize its rustls connection here: the connection's advertised ALPN
        // list is fixed at construction, and the selector's answer is not known
        // until the ClientHello arrives. `do_unwrap` realizes it (after running
        // `engine_apply_alpn_selector`), which is also when a server engine has
        // anything to do — JSSE's server-side `beginHandshake()` cannot produce
        // a byte before it has seen the hello either.
        // `engine_objref_key` calls `ctx.identity_hash_code`; computing it
        // BEFORE the guard keeps `engine_alpn_selector_table` off the
        // re-entrant path, which is what its `LockLevel` claims.
        let alpn_key = engine_objref_key(ctx, this);
        let defer_for_alpn = with_engine(id, |s| !s.is_client && s.conn.is_none()).unwrap_or(false)
            && engine_alpn_selector_table().lock().contains_key(&alpn_key);
        if defer_for_alpn {
            with_engine(id, |s| {
                s.alpn_selection_deferred = true;
            });
            return Ok(None);
        }
        // Drop the registry lock BEFORE reporting a failure: `engine_begin_failure`
        // allocates a Java exception, and allocating while holding this lock is the
        // GC self-deadlock `EngineState::trust_managers_ctx_key`'s doc describes.
        let res = with_engine(id, engine_begin).unwrap_or(Ok(()));
        if let Err(e) = res {
            return Err(engine_begin_failure(ctx, e));
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // getDelegatedTask() — hand over the work this engine deferred, once.
    //
    // Was not registered at all before, so a caller that followed the
    // NEED_TASK this engine can already report (the fallthrough at the end of
    // `handshake_status_of`) had no way to satisfy it.
    r.register(
        cls_impl,
        "getDelegatedTask",
        "()Ljava/lang/Runnable;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let hand_out = claim_delegated_task(id);
            if crate::nbflags().dbg_tls_hs_ok {
                eprintln!(
                    "[dbg-tls-task] thread={:?} getDelegatedTask id={} hand_out={}",
                    std::thread::current().id(),
                    id,
                    hand_out
                );
            }
            if !hand_out {
                // JSSE answers null once the queue is drained, and netty's
                // in-line `runDelegatedTasks` loop terminates on exactly that.
                return Ok(Some(Value::Object(None)));
            }
            let task = try_alloc_concurrent_synthetic(ctx, "java/lang/Runnable", 1)?;
            ctx.set_field(task, 0, Value::Int(id));
            Ok(Some(Value::Object(Some(task))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // The task itself. Registered on `java.lang.Runnable` because that is the
    // class the object above wears — nothing else in this VM allocates a bare
    // `Runnable`, so the interception cannot capture an application's own.
    //
    // **It never throws.** The task runs on somebody else's thread — that is
    // its whole point — and callers do not treat it as a call site that can
    // fail. Throwing left `delegate=true` blind to failures `delegate=false`
    // saw: `SSLEngineTest.testClientHostnameValidationFail` failed on exactly
    // its three `delegate=true` parameterisations, and `testIncompatibleCiphers`
    // spun forever because neither engine ever learned the handshake was over.
    // Failures go onto `EngineState::deferred_handshake_error`, which exists
    // for this shape and which `do_wrap` raises once rustls's fatal alert has
    // gone out — so the peer learns why before this side does.
    r.register("java/lang/Runnable", "run", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = match ctx.get_field(this, 0) {
            Value::Int(v) => v,
            _ => return Ok(None),
        };
        // Both failure shapes are carried OUT from under the registry lock
        // before they are raised: building a Java exception allocates, and
        // allocating under this lock is the GC self-deadlock
        // `EngineState::trust_managers_ctx_key` documents.
        let mut pump_job: Option<Vec<u8>> = None;
        let outcome = {
            let cell = engine_cell(id);
            let mut g = cell.as_ref().map(|c| c.lock());
            match g.as_deref_mut() {
                None => Ok(None),
                Some(s) => {
                    s.delegated_task = DelegatedTask::None;
                    // On a SERVER engine this builds the rustls config; on a
                    // CLIENT the connection already exists and this is a no-op.
                    match engine_begin_if_needed(s) {
                        Err(e) => Err(e),
                        Ok(()) => {
                            // The work proper: feed rustls the handshake flight
                            // the deferring `unwrap` took out of the caller's
                            // buffer, and process it. This is the expensive
                            // half of a handshake — certificate verification,
                            // key agreement, signing — and it is what JSSE
                            // defers. Doing it HERE rather than leaving it for
                            // a later `unwrap` matters: a caller that feeds the
                            // engine only when it has bytes has none left to
                            // feed, and both peers stall waiting for each
                            // other. See `DelegatedTask`.
                            let staged = std::mem::take(&mut s.deferred_inbound);
                            if staged.is_empty() {
                                Ok(None)
                            } else {
                                // The CLIENT half of the shared TLS 1.2 session
                                // id is peeked out of the ServerHello RECORD,
                                // before rustls consumes it — `do_unwrap`'s
                                // record loop does exactly this, and deferring
                                // routes the record past that loop.
                                // `SSLEngineTest.testSSLSessionId` compares the
                                // two engines' ids byte for byte and read
                                // "array contents differ at index [0]" on all
                                // six of its running parameterisations.
                                if s.negotiated_session_id.is_empty() {
                                    let mut off = 0usize;
                                    while off + 5 <= staged.len() {
                                        let len = ((staged[off + 3] as usize) << 8)
                                            | staged[off + 4] as usize;
                                        let end = off + 5 + len;
                                        if end > staged.len() {
                                            break;
                                        }
                                        if let Some(sid) =
                                            peek_server_hello_session_id(&staged[off..end])
                                        {
                                            s.negotiated_session_id = sid;
                                            break;
                                        }
                                        off = end;
                                    }
                                }
                                // nio21 wave 13 (L4): the pump runs below, OUTSIDE
                                // this lock, with the connection on loan and the
                                // native context published, as `do_unwrap`'s record
                                // loop runs it. rustls consults the application's
                                // `TrustManager` from inside `process_new_packets`;
                                // with no context published the verifier accepted
                                // and the post-handshake gate asked Java after the
                                // client's `Finished` was already queued, so the
                                // rejection alert went out under the application
                                // keys and a rustls peer could not decrypt it
                                // (`NioConf19b tmthrows`, `--compatible`).
                                pump_job = Some(staged);
                                Ok(None)
                            }
                        }
                    }
                }
            }
        };
        let outcome = match (outcome, pump_job) {
            (Ok(None), Some(staged)) => {
                // Feed rustls the flight the deferring `unwrap` took out of the
                // caller's buffer (see the note in the block above).
                let res = {
                    let mut checkout = ConnCheckout::take(id);
                    let _presented_sink = PresentedSink::enter(
                        with_engine(id, |s| s.client_cert_presented.clone()).flatten(),
                    );
                    let _active_ctx = set_active_native_context(ctx);
                    let (_active_engine, engine_pin) = set_active_engine_binding(ctx, id, this);
                    let _unpin = UnpinOnDrop { base: engine_pin };
                    let mut carry =
                        with_engine(id, |s| std::mem::take(&mut s.inbound)).unwrap_or_default();
                    let res = match checkout.conn.as_mut() {
                        Some(conn) => pump_conn_raw(conn, &mut carry, &staged),
                        None => Ok((0, Vec::new())),
                    };
                    drop(checkout);
                    with_engine(id, |s| s.inbound = carry);
                    res
                };
                match res {
                    Ok((_, pt)) => {
                        // Any plaintext belongs to the caller's next unwrap;
                        // `do_unwrap`'s Step 0 serves this buffer before it
                        // touches the network again.
                        //
                        // NOT `engine_capture_negotiation` -- the task runs
                        // mid-handshake, and capturing there latched a session id
                        // that was not the negotiated one
                        // (`SSLEngineTest.testSSLSessionId`). `do_wrap` and
                        // `do_unwrap` both capture at the right moment already.
                        with_engine(id, |s| s.plaintext_pending.extend_from_slice(&pt));
                        Ok(None)
                    }
                    Err(e) => {
                        // Handed to the engine, not thrown. See the note above
                        // `r.register("java/lang/Runnable", ...)`.
                        let msg = handshake_error_message(&e);
                        with_engine(id, |s| {
                            s.deferred_handshake_error =
                                Some((jsse_handshake_exception_class(&e), msg.clone()));
                        });
                        Ok(Some(msg))
                    }
                }
            }
            (outcome, _) => outcome,
        };
        if crate::nbflags().dbg_tls_hs_ok {
            eprintln!(
                "[dbg-tls-task] thread={:?} task.run id={} outcome={}",
                std::thread::current().id(),
                id,
                match &outcome {
                    Ok(None) => "ok".to_string(),
                    Ok(Some(msg)) => format!("deferred: {msg}"),
                    Err(e) => format!("begin-failed: {e}"),
                }
            );
        }
        match outcome {
            // Both arms return normally: whatever went wrong is on the engine
            // now, and the caller's next `wrap` raises it.
            Ok(_) => Ok(None),
            Err(e) => {
                with_engine(id, |s| {
                    s.deferred_handshake_error =
                        Some(("javax/net/ssl/SSLHandshakeException", e.clone()));
                });
                Ok(None)
            }
        }
    }, cratonvm_native_api::NativeKind::Bridge);

    // wrap(ByteBuffer src, ByteBuffer dst)
    r.register(
        cls_impl,
        "wrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        wrap_single, cratonvm_native_api::NativeKind::Bridge,
    );

    // wrap(ByteBuffer[] srcs, ByteBuffer dst)
    r.register(
        cls_impl,
        "wrap",
        "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        wrap_array, cratonvm_native_api::NativeKind::Bridge,
    );

    // wrap(ByteBuffer[] srcs, int offset, int length, ByteBuffer dst)
    r.register(
        cls_impl,
        "wrap",
        "([Ljava/nio/ByteBuffer;IILjava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        wrap_array_offset, cratonvm_native_api::NativeKind::Bridge,
    );

    // unwrap(ByteBuffer src, ByteBuffer dst)
    r.register(
        cls_impl,
        "unwrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        unwrap_single, cratonvm_native_api::NativeKind::Bridge,
    );

    // unwrap(ByteBuffer src, ByteBuffer[] dsts)
    r.register(
        cls_impl,
        "unwrap",
        "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        unwrap_array, cratonvm_native_api::NativeKind::Bridge,
    );

    // unwrap(ByteBuffer src, ByteBuffer[] dsts, int offset, int length)
    r.register(
        cls_impl,
        "unwrap",
        "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;II)Ljavax/net/ssl/SSLEngineResult;",
        unwrap_array_offset, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls_impl,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let hs = with_engine(id, |s| handshake_status_of(s)).unwrap_or(HS_NOT_HANDSHAKING_R);
            // Return the REAL enum singleton so `engine.getHandshakeStatus() ==
            // NEED_WRAP` etc. in the connector's handshake loop work.
            Ok(Some(real_handshake_status_enum(ctx, hs)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(cls_impl, "closeOutbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        // gc-common w12-a: a closed engine never handshakes, so its ALPN
        // selector can no longer be consulted and stops being a root.
        alpn_selector_release_root(engine_objref_key(ctx, this));
        if crate::nbflags().dbg_tls_hs_ok {
            eprintln!(
                "[dbg-tls-hs] thread={:?} JAVA_CALLED closeOutbound() id={}",
                std::thread::current().id(),
                id
            );
        }
        with_engine(id, |s| {
            s.closed_outbound = true;
            if let Some(c) = s.conn.as_mut() {
                c.send_close_notify();            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // JSSE's `closeInbound()` is not a plain setter: `TransportContext
    // .initiateInboundClose` throws `SSLException("closing inbound before
    // receiving peer's close_notify")` when a handshake is IN PROGRESS
    // (`handshakeContext != null`) and no `close_notify` has arrived — a
    // truncation attack is indistinguishable from a peer that simply stopped
    // talking, so the contract is to complain rather than to accept a
    // half-open connection silently.
    //
    // The gate is "a handshake has begun and has not finished", which is what
    // makes the two netty tests that straddle it agree:
    // `testCloseInboundAfterBeginHandshake` calls `beginHandshake()` first and
    // asserts a throw, while `testBeginHandshakeAfterEngineClosed` calls
    // `closeInbound()` on a never-handshaked engine and asserts it returns
    // normally. The inbound flag is set either way — the exception reports what
    // happened, it does not veto the close.
    r.register(cls_impl, "closeInbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        // gc-common w12-a: as in `closeOutbound`.
        alpn_selector_release_root(engine_objref_key(ctx, this));
        let complain = with_engine(id, |s| {
            !s.closed_inbound
                && match s.conn.as_ref() {
                    Some(c) => c.is_handshaking() && !c.peer_has_closed(),
                    None => false,
                }
        })
        .unwrap_or(false);
        with_engine(id, |s| {
            s.closed_inbound = true;
        });
        if complain {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLException",
                "closing inbound before receiving peer's close_notify",
            ));
        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // JSSE's `isInboundDone()` is "no more inbound data will be accepted",
    // which is true both when the application called `closeInbound()` AND when
    // the PEER's `close_notify` has been received — `SSLEngineTest
    // .testCloseNotifySequence` asserts exactly the second case
    // (`assertTrue(server.isInboundDone())` after the server unwrapped the
    // client's close_notify, with no `closeInbound()` call anywhere).
    r.register(cls_impl, "isInboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let v = with_engine(id, |s| {
            s.closed_inbound
                || s.conn
                    .as_ref()
                    .map(|c| c.peer_has_closed())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);

    // ...and `isOutboundDone()` is "the close_notify has been SENT", not
    // "closeOutbound() was called". `closeOutbound()` only QUEUES the alert;
    // until a `wrap()` drains it the engine still has outbound work to do and
    // JSSE answers false. `testCloseNotifySequence` asserts both halves in a
    // row — `assertFalse(client.isOutboundDone())` immediately after
    // `closeOutbound()`, then `assertTrue(...)` after the wrap that drains it —
    // and the flag-only answer failed the first of them
    // (`expected: <false> but was: <true>`).
    r.register(cls_impl, "isOutboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(ctx, this);
        let v = with_engine(id, |s| {
            s.closed_outbound
                && s.outbound.is_empty()
                && !s.conn.as_ref().map(|c| c.wants_write()).unwrap_or(false)
        })
        .unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);

    // getSession() — synthetic SSLSession with negotiated cipher/protocol/alpn.
    r.register(
        cls_impl,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            Ok(Some(Value::Object(Some(engine_session_for(
                ctx,
                this,
                id,
                SessionDoor::Current,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getHandshakeSession() — real `SSLEngineImpl.getHandshakeSession()` reads
    // a real, JDK-internal `conContext` field that CratonVM's engine never
    // populates (handshake state lives entirely in `EngineState`/
    // `engine_registry()`, not on the real bytecode object) — un-intercepted,
    // it NPEs ("Cannot read field \"handshakeContext\" because
    // \"this.conContext\" is null"). Found via Jetty's
    // `SslConnection.getBufferSize()` -> `getApplicationBufferSize()` ->
    // `sslEngine.getHandshakeSession()`, called while sizing buffers for a
    // brand-new client connection — i.e. BEFORE `beginHandshake()`/`wrap()`
    // ever run, so `state.conn` is still `None` at this point. Jetty's own
    // exception handling here (`ManagedSelector$Accept.run()`'s
    // catch-Throwable) silently drops the failure (logs at DEBUG only, never
    // reaches the connection's promise), which is why this specific NPE
    // manifested as an indefinite hang/silent-exit crash rather than a
    // visible test failure — see
    // http-client-connector-teardown-hang-crash-FIXED.md.
    // Real JDK's `getHandshakeSession()` returns the session being negotiated,
    // and NULL outside a handshake — which the sentence above already said and
    // the code below then did not do. It returned a populated synthetic
    // session unconditionally, so it was right only in the middle window and
    // wrong on BOTH sides, including for the exact caller this comment names:
    // Jetty asks before `beginHandshake()`, i.e. precisely where the correct
    // answer is null.
    //
    // E12 — measured, HotSpot 25.0.3+9-LTS, stepping a client/server engine
    // pair (`scratchpad/e12/E12HandshakeSession.java`, E12-1 §1):
    //
    //     step  0  NEED_WRAP        client getHandshakeSession() = null
    //     step  5  NEED_WRAP        client getHandshakeSession() = populated
    //     step 10  NOT_HANDSHAKING  client getHandshakeSession() = null
    //
    // Null before, populated during, null after.
    //
    // WHY RETURNING NULL CANNOT BREAK THE JETTY CALLER THIS COMMENT NAMES,
    // argued from the oracle rather than from Jetty's source (which is not
    // checked out on this host): HotSpot returns null here for every
    // brand-new connection, which is exactly when Jetty calls it. Any caller
    // that works on HotSpot therefore already tolerates null at this call
    // site — otherwise it would NPE on real JSSE on every connection. The
    // original CratonVM defect this registration exists for was a *different*
    // NPE (real `SSLEngineImpl.getHandshakeSession()` dereferencing a
    // `conContext` this VM never populates); it is fixed by the registration
    // EXISTING, not by what the registration returns.
    r.register(
        cls_impl,
        "getHandshakeSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            // `conn` is `None` until `beginHandshake` realizes the rustls
            // connection, and `is_handshaking()` flips false once the
            // handshake completes — so this is non-null over exactly the
            // window HotSpot measures above, and null on both sides of it.
            //
            // `in_trust_check()` is the one state rustls does not call
            // "handshaking" while JSSE still counts it as inside the window,
            // and the socket-door oracle measures it directly (E31-1 §1,
            // `docs/known-issues/jdk-only/`): `getHandshakeSession()` answers a
            // POPULATED session inside `checkServerTrusted(chain, auth,
            // Socket)` and `null` again inside `HandshakeCompletedListener`.
            // dev's version of this handler reached for the same flag, from the
            // other side of the predicate.
            let mid_handshake = with_engine(id, |s| {
                s.conn.as_ref().map(|c| c.is_handshaking()).unwrap_or(false)
            })
            .unwrap_or(false)
                || in_trust_check();
            if !mid_handshake {
                return Ok(Some(Value::Object(None)));
            }
            // `engine_session_for`, not a fresh `build_synthetic_ssl_session` on
            // every call: the session OBJECT's identity is what
            // `putValue`/`getValue`, `invalidate()` and `getCreationTime()` are
            // answered from — see `engine_session_table`.
            Ok(Some(Value::Object(Some(engine_session_for(
                ctx,
                this,
                id,
                SessionDoor::Handshake,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getApplicationProtocol() — return negotiated ALPN (or empty string)
    r.register(
        cls_impl,
        "getApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let alpn = with_engine(id, |s| s.negotiated_alpn.clone())
                .flatten()
                .unwrap_or_default();
            let s = ctx.create_string(&alpn);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // setApplicationProtocols — used by SSLEngine path (not just SSLParameters)
    r.register(
        cls_impl,
        "setApplicationProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let mut list: Vec<Vec<u8>> = Vec::new();
            if let Some(Value::Object(Some(arr))) = args.get(1) {
                let len = ctx.array_length(*arr);
                for i in 0..len {
                    if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                        if let Some(t) = ctx.read_string(s) {
                            list.push(t.into_bytes());
                        }
                    }
                }
            }
            with_engine(id, |s| {
                s.alpn_protocols = list;            });
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // getHandshakeApplicationProtocol — same as getApplicationProtocol for our path.
    r.register(
        cls_impl,
        "getHandshakeApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            let alpn = with_engine(id, |s| s.negotiated_alpn.clone())
                .flatten()
                .unwrap_or_default();
            let s = ctx.create_string(&alpn);
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

// -- wrap/unwrap closures (split out for arity / arg shapes) -----------------

pub fn wrap_single(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = match args.get(1) {
        Some(Value::Object(Some(b))) => Some(*b),
        _ => None,
    };
    let dst = match args.get(2) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_engine_result(
                ctx,
                SR_BUFFER_OVERFLOW,
                HS_NEED_WRAP_R,
                0,
                0,
            )?))))
        }
    };
    Ok(do_wrap(ctx, this, src.into_iter().collect(), dst)?)
}

pub fn wrap_array(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = obj_arg(args, 0)?;
    let srcs_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => Some(*a),
        _ => None,
    };
    let dst = match args.get(2) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_engine_result(
                ctx,
                SR_BUFFER_OVERFLOW,
                HS_NEED_WRAP_R,
                0,
                0,
            )?))))
        }
    };
    let mut srcs: Vec<ObjectRef> = Vec::new();
    if let Some(arr) = srcs_arr {
        let len = ctx.array_length(arr);
        for i in 0..len {
            if let Value::Object(Some(b)) = ctx.get_array_element(arr, i) {
                srcs.push(b);
            }
        }
    }
    Ok(do_wrap(ctx, this, srcs, dst)?)
}

pub fn wrap_array_offset(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = obj_arg(args, 0)?;
    let srcs_arr = match args.get(1) {
        Some(Value::Object(Some(a))) => Some(*a),
        _ => None,
    };
    let off = args.get(2).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len_arg = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let dst = match args.get(4) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_engine_result(
                ctx,
                SR_BUFFER_OVERFLOW,
                HS_NEED_WRAP_R,
                0,
                0,
            )?))))
        }
    };
    let mut srcs: Vec<ObjectRef> = Vec::new();
    if let Some(arr) = srcs_arr {
        let total = ctx.array_length(arr);
        let end = off.saturating_add(len_arg).min(total);
        for i in off..end {
            if let Value::Object(Some(b)) = ctx.get_array_element(arr, i) {
                srcs.push(b);
            }
        }
    }
    Ok(do_wrap(ctx, this, srcs, dst)?)
}

pub fn do_wrap(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    this: ObjectRef,
    srcs: Vec<ObjectRef>,
    dst: ObjectRef,
) -> cratonvm_types::error::MethodCallResult {
    // ENTRY pins, before the first `ctx` call. Everything below can allocate
    // and collect — `engine_id_or_alloc` first of all — and a pin taken later
    // in the body pins whatever `this` has already decayed to, which a pin
    // cannot repair. Measured: PIN-STALE named this exact receiver
    // (`sun/security/ssl/SSLEngineImpl`) being pinned at an already-dead
    // address by `engine_consult_trust_managers` downstream.
    let this_entry_pin = ctx.pin_native_root(this);
    let dst_entry_pin = ctx.pin_native_root(dst);
    let this = ctx.read_native_pin(this_entry_pin, this);
    let id = engine_id_or_alloc(ctx, this);
    let __dbg_hs = crate::nbflags().dbg_tls_hs_ok;
    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_wrap ENTER id={}",
            std::thread::current().id(),
            id
        );
    }

    // Closed outbound — but NOT a short circuit.
    //
    // `closeOutbound()` queues a `close_notify` on the rustls connection, and
    // a fatal alert may be queued there too (a `TrustManager` rejection — see
    // `engine_run_trust_check`). The wrap that FOLLOWS the close is the call
    // JSSE specifies as the one that emits that record: `wrap` returns
    // `Status.CLOSED` with `bytesProduced` equal to the alert's length, and
    // only once the queue is empty does it produce nothing.
    //
    // This used to return `CLOSED, produced=0` immediately, so the alert was
    // generated, encrypted, and then left in rustls's write queue forever.
    // Measured consequences, all one defect:
    //   * netty's `CloseNotifyTest` / `ApplicationProtocolNegotiationHandlerTest`
    //     see an EMPTY outbound buffer where a close_notify record belongs
    //     (`assertCloseNotify`: "0 to be greater than or equal to 7");
    //   * `ParameterizedSslHandlerTest.testAlertProducedAndSend` blocks
    //     forever in `awaitUninterruptibly()` — the peer is waiting for an
    //     alert that is sitting in this queue.
    //
    // The drain below is the ordinary path; `closed` only suppresses reading
    // application data from `srcs` (JSSE consumes nothing after close) and
    // forces the reported status to CLOSED.
    let closed = with_engine(id, |s| s.closed_outbound).unwrap_or(false);
    if closed {
        let no_conn = with_engine(id, |s| s.conn.is_none()).unwrap_or(true);
        let nothing_queued = with_engine(id, |s| s.outbound.is_empty()).unwrap_or(true);
        if no_conn && nothing_queued {
            // Closed before anything was ever negotiated: there is no record
            // layer to encode an alert with, so CLOSED with nothing produced
            // is the whole truth. Realizing a connection here would start a
            // handshake for a closed engine.
            if __dbg_hs {
                eprintln!(
                    "[dbg-tls-hs] thread={:?} do_wrap id={} CLOSED_NO_CONNECTION",
                    std::thread::current().id(),
                    id
                );
            }
            let result = alloc_engine_result(ctx, SR_CLOSED, HS_NOT_HANDSHAKING_R, 0, 0);
            return Ok(Some(Value::Object(Some(result?))));
        }
        if __dbg_hs {
            eprintln!(
                "[dbg-tls-hs] thread={:?} do_wrap id={} CLOSED_OUTBOUND_DRAIN",
                std::thread::current().id(),
                id
            );
        }
    }

    // A server engine with a handshake ALPN selector
    // (`setHandshakeApplicationProtocolSelector`) that has not seen the
    // ClientHello yet: JSSE's `wrap` answers OK / NEED_UNWRAP with nothing
    // produced (a server has nothing to say first), and the selector runs on
    // the ClientHello (`engine_apply_alpn_selector`, in `do_unwrap`). Realizing
    // the rustls connection here fixed its ALPN list before that, so a caller
    // that wraps before it unwraps never had its selector asked (nio20 wave 3,
    // L1; `NioConf19b` `alpn-selector`). Scoped to engines WITH a selector, so
    // every other server engine's first wrap is unchanged.
    if !closed {
        let unrealized_server =
            with_engine(id, |s| !s.is_client && s.conn.is_none()).unwrap_or(false);
        if unrealized_server {
            let this_now = ctx.read_native_pin(this_entry_pin, this);
            let key = engine_objref_key(ctx, this_now);
            if engine_alpn_selector_table().lock().contains_key(&key) {
                let result = alloc_engine_result(ctx, SR_OK, HS_NEED_UNWRAP_R, 0, 0);
                return Ok(Some(Value::Object(Some(result?))));
            }
        }
    }

    {
        // The lock is dropped before the failure is reported: `engine_begin_failure`
        // allocates the Java exception, and allocating under the engine's lock
        // (the registry's write lock before gc-common w11-a) is the GC
        // self-deadlock `EngineState::trust_managers_ctx_key` documents.
        let res = with_engine(id, |s| {
            if s.conn.is_none() {
                engine_begin(s)
            } else {
                Ok(())
            }
        })
        .unwrap_or(Ok(()));
        if let Err(e) = res {
            if crate::nbflags().dbg_tls_hs_ok {
                eprintln!(
                    "[dbg-tls-hs] thread={:?} do_unwrap/do_wrap id={} RETURN(engine_begin ERROR) err={}",
                    std::thread::current().id(), id, e
                );
            }
            return Err(engine_begin_failure(ctx, e));
        }
    }

    // Step 1: read app data from src ByteBuffers (only once the handshake is
    // OVER as far as the CALLER is concerned).
    //
    // The gate is `handshake_finished_reported`, not `!conn.is_handshaking()`.
    // Those two are not the same instant: `handshake_status_of` deliberately
    // keeps answering NEED_WRAP after `is_handshaking()` flips false, until the
    // engine's own final flight has been drained (the TLS 1.2 server-flight fix
    // in `handshake_status_of`). A caller that correctly obeys that NEED_WRAP
    // calls `wrap(src, dst)` while still handshaking from its point of view --
    // and JSSE's contract says such a wrap consumes NOTHING from `src`.
    //
    // Gating on `!is_handshaking()` alone made that wrap treat `src` as
    // application data. Tomcat's WebSocket client hands it a 16921-byte
    // `AsyncChannelWrapperSecure.DUMMY`, so the engine drained 16384 bytes of
    // zeros out of it, ENCRYPTED them onto the wire mid-upgrade, and reported
    // `bytesConsumed=16384` -- which is exactly the invariant
    // `AsyncChannelWrapperSecure.checkResult` asserts, so the connect died with
    // "Bytes were consumed from the input during a write". `DUMMY` is `static`
    // and nobody rewinds it, so the position damage leaked into every later
    // connection in the same JVM (the second engine found only 537 bytes left).
    //
    // Tomcat's server-side NIO path passes an empty buffer to its handshake
    // wraps, which is why nothing but the WebSocket client ever noticed.
    let mut app_bytes = Vec::new();
    let mut consumed_app = 0usize;
    // `!closed`: JSSE consumes nothing from `srcs` once `closeOutbound()` has
    // been called — the only thing left to produce is the queued alert.
    let needs_app_data = !closed
        && with_engine(id, |s| {
            s.handshake_finished_reported
                && s.conn
                    .as_ref()
                    .map(|c| !c.is_handshaking())
                    .unwrap_or(false)
        })
        .unwrap_or(false);
    if needs_app_data {
        for bb in &srcs {
            let n = bb_read_into(ctx, *bb, &mut app_bytes, 16384);
            consumed_app += n;
            if app_bytes.len() >= 16384 {
                break;
            }
        }
    }

    // Step 2: pump rustls + drain into dst.
    let dst_view = bb_view(ctx, dst);
    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_wrap id={} DST {}",
            std::thread::current().id(),
            id,
            bb_describe(ctx, dst, &dst_view)
        );
    }
    let dst_remaining = dst_view.lim.saturating_sub(dst_view.pos);

    let (consumed_inner, status, hs, drained, pending_trust_check, deferred_failure) = {
        let cell = match engine_cell(id) {
            Some(cell) => cell,
            None => {
                return Err(RuntimeError::IOException {
                    message: "engine handle missing".into(),
                }
                .into())
            }
        };
        let mut g = cell.lock();
        let s: &mut EngineState = &mut g;
        let (cons, _produced) = engine_wrap_pump(s, &app_bytes, dst_remaining);
        // The SERVER side of the shared TLS 1.2 session id: the ServerHello is
        // the first record this engine ever writes, and it is still in
        // `outbound` here — the drain below removes it. See
        // `peek_server_hello_session_id`.
        if !s.is_client && s.negotiated_session_id.is_empty() {
            if let Some(sid) = peek_server_hello_session_id(&s.outbound) {
                s.negotiated_session_id = sid;
            }
        }
        // Drain only complete TLS records. A partial record written to the
        // channel cannot be recovered by a later wrap call.
        let take = complete_tls_record_prefix(&s.outbound, dst_remaining);
        let drained: Vec<u8> = s.outbound.drain(0..take).collect();

        // Report overflow only when the destination cannot hold even the next
        // complete record. If at least one record was emitted, returning OK
        // lets Tomcat flush it and call wrap again for the remaining record.
        let status = if drained.is_empty() && !s.outbound.is_empty() {
            SR_BUFFER_OVERFLOW
        } else if closed {
            // JSSE: every wrap after `closeOutbound()` reports CLOSED,
            // including the one that carries the close_notify / alert record.
            // netty writes `out` BEFORE it looks at the status, so reporting
            // CLOSED alongside a non-zero `bytesProduced` is exactly what gets
            // the record onto the wire and then stops the wrap loop.
            SR_CLOSED
        } else {
            SR_OK
        };
        engine_capture_negotiation(s);
        // A closed engine is not handshaking; it either still owes the peer
        // the rest of its alert (NEED_WRAP, so a caller that loops keeps
        // pulling) or it owes nothing.
        let hs = if closed {
            // `wants_write()` too: rustls may still be holding the alert that
            // no `wrap` has drained into `outbound` yet.
            if s.outbound.is_empty() && !s.conn.as_ref().is_some_and(|c| c.wants_write()) {
                HS_NOT_HANDSHAKING_R
            } else {
                HS_NEED_WRAP_R
            }
        } else {
            handshake_status_of(s)
        };
        if hs == HS_FINISHED_R {
            s.handshake_finished_reported = true;
        }
        // The handshake failure this engine detected on a previous `unwrap`,
        // once the fatal alert it queued has actually gone out. Taking it only
        // when nothing is left to write is what keeps the peer's copy of the
        // alert intact — see `EngineState::deferred_handshake_error`.
        //
        // `drained.is_empty()` as well, and that is the load-bearing half: a
        // wrap that raises cannot also deliver. netty advances its out-buffer's
        // writerIndex from `result.bytesProduced()`, and a throwing `wrap` has
        // no result to read it from — the buffer reads back empty, is released,
        // and the alert this very call had just drained into it dies there. The
        // peer then learns only that the channel closed, which is exactly what
        // `testHandshakeFailureCipherMissmatch{TLSv12,TLSv13}Jdk` measured on
        // the CLIENT side (SslHandlerTest:1670,
        // `StacklessClosedChannelException` where an `SSLException` belongs).
        //
        // `handshake_status_of` keeps answering NEED_WRAP while the failure is
        // pending, so the caller comes back for the wrap that produces nothing
        // — and that one raises.
        let deferred_failure = if s.deferred_handshake_error.is_some()
            && drained.is_empty()
            && s.outbound.is_empty()
            && !s.conn.as_ref().is_some_and(|c| c.wants_write())
        {
            s.deferred_handshake_error.take()
        } else {
            None
        };
        // Extract-only — see `engine_take_pending_trust_check`'s doc for why
        // the actual Java call must happen after this lock is dropped.
        let pending_trust_check = engine_take_pending_trust_check(id, s);
        (
            cons,
            status,
            hs,
            drained,
            pending_trust_check,
            deferred_failure,
        )
    };
    // `engine_run_trust_check` runs the application's TrustManager — arbitrary
    // Java that allocates and can collect for as long as it likes. `this` and
    // `dst` are raw `ObjectRef`s from the argument slots: the caller's operand
    // slots are roots and get remapped, these copies are not, and `dst` is
    // WRITTEN THROUGH below. Pin both across the callback and re-derive them
    // after it — the idiom the retired `unpinned-native-locals-audit` write-up
    // applies, here over the widest window of that shape in this file.
    let mut this = this;
    let mut dst = dst;
    if let Some(pending) = pending_trust_check {
        this = ctx.read_native_pin(this_entry_pin, this);
        engine_run_trust_check(ctx, pending, Some(this))?;
    }
    let _ = this;

    // Step 3: write the drained bytes into dst.
    let produced = if !drained.is_empty() {
        dst = ctx.read_native_pin(dst_entry_pin, dst);
        bb_write_from(ctx, dst, &drained)
    } else {
        0
    };
    if let Some((cls, msg)) = deferred_failure {
        // The alert is in `dst` (or already went out on an earlier wrap), so
        // the peer learns why; this side now learns it too.
        return Err(crate::phases_early::throw_jca_exc(ctx, cls, &msg));
    }

    let total_consumed = consumed_app.max(consumed_inner) as i32;
    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_wrap id={} RESULT status={} hs={} consumed={} produced={}",
            std::thread::current().id(),
            id,
            status_name(status),
            hs_name(hs),
            total_consumed,
            produced
        );
    }
    let result = alloc_engine_result(ctx, status, hs, total_consumed, produced as i32);
    Ok(Some(Value::Object(Some(result?))))
}

pub fn unwrap_single(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_engine_result(
                ctx,
                SR_BUFFER_UNDERFLOW,
                HS_NEED_UNWRAP_R,
                0,
                0,
            )?))))
        }
    };
    let dst = match args.get(2) {
        Some(Value::Object(Some(b))) => Some(*b),
        _ => None,
    };
    Ok(do_unwrap(ctx, this, src, dst.into_iter().collect())?)}

pub fn unwrap_array(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_engine_result(
                ctx,
                SR_BUFFER_UNDERFLOW,
                HS_NEED_UNWRAP_R,
                0,
                0,
            )?))))
        }
    };
    let mut dsts: Vec<ObjectRef> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(2) {
        let len = ctx.array_length(*arr);
        for i in 0..len {
            if let Value::Object(Some(b)) = ctx.get_array_element(*arr, i) {
                dsts.push(b);
            }
        }
    }
    Ok(do_unwrap(ctx, this, src, dsts)?)
}

pub fn unwrap_array_offset(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    let this = obj_arg(args, 0)?;
    let src = match args.get(1) {
        Some(Value::Object(Some(b))) => *b,
        _ => {
            return Ok(Some(Value::Object(Some(alloc_engine_result(
                ctx,
                SR_BUFFER_UNDERFLOW,
                HS_NEED_UNWRAP_R,
                0,
                0,
            )?))))
        }
    };
    let off = args.get(3).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let len_arg = args.get(4).and_then(|v| v.as_int()).unwrap_or(0).max(0) as usize;
    let mut dsts: Vec<ObjectRef> = Vec::new();
    if let Some(Value::Object(Some(arr))) = args.get(2) {
        let total = ctx.array_length(*arr);
        let end = off.saturating_add(len_arg).min(total);
        for i in off..end {
            if let Value::Object(Some(b)) = ctx.get_array_element(*arr, i) {
                dsts.push(b);
            }
        }
    }
    Ok(do_unwrap(ctx, this, src, dsts)?)
}

pub fn do_unwrap(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    this: ObjectRef,
    src: ObjectRef,
    dsts: Vec<ObjectRef>,
) -> cratonvm_types::error::MethodCallResult {
    // ENTRY pins, before the first `ctx` call. Everything below can allocate
    // and collect — `engine_id_or_alloc` first of all — and a pin taken later
    // in the body pins whatever `this` has already decayed to, which a pin
    // cannot repair. Measured: PIN-STALE named this exact receiver
    // (`sun/security/ssl/SSLEngineImpl`) being pinned at an already-dead
    // address by `engine_consult_trust_managers` downstream.
    let this_entry_pin = ctx.pin_native_root(this);
    let src_entry_pin = ctx.pin_native_root(src);
    let dst_entry_pins: Vec<usize> = dsts.iter().map(|d| ctx.pin_native_root(*d)).collect();
    let this = ctx.read_native_pin(this_entry_pin, this);
    let id = engine_id_or_alloc(ctx, this);
    let __dbg_hs = crate::nbflags().dbg_tls_hs_ok;
    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_unwrap ENTER id={}",
            std::thread::current().id(),
            id
        );
    }

    let closed = with_engine(id, |s| s.closed_inbound).unwrap_or(false);
    if closed {
        if __dbg_hs {
            eprintln!(
                "[dbg-tls-hs] thread={:?} do_unwrap id={} CLOSED_INBOUND_SHORT_CIRCUIT",
                std::thread::current().id(),
                id
            );
        }
        let result = alloc_engine_result(ctx, SR_CLOSED, HS_NOT_HANDSHAKING_R, 0, 0);
        return Ok(Some(Value::Object(Some(result?))));
    }

    // Server-side ALPN selection has to happen BEFORE the rustls connection
    // exists — its advertised protocol list is fixed at construction — and the
    // choice needs the ClientHello, so it runs here, ahead of `engine_begin`.
    // A no-op for client engines, for an already-realized connection, and for
    // an engine with no selector installed.
    engine_apply_alpn_selector(ctx, id, this, src)?;

    // Step 1+2: record-oriented unwrap. Feed rustls only COMPLETE TLS records
    // from `src` whose decrypted plaintext fits the caller's dst buffers, and
    // advance `src` past exactly those records. Incomplete records, or records
    // beyond what the dst can hold, are LEFT in `src` (the caller's netInBuffer)
    // so the caller re-feeds them on its next unwrap.
    //
    // This is the crux of correct SSLEngine semantics over rustls: never decrypt
    // more plaintext than the caller's buffer can take and stash the excess in
    // our own buffer — the caller (Tomcat) cannot see that buffer and blocks
    // reading the socket for body bytes that already arrived, yielding
    // java.net.SocketTimeoutException → HTTP 400 on large request bodies.
    let dst_cap: usize = dsts
        .iter()
        .map(|d| {
            let v = bb_view(ctx, *d);
            v.lim.saturating_sub(v.pos)
        })
        .sum();

    // Step 0: serve any plaintext that overflowed a previous unwrap's dst FIRST,
    // WITHOUT consuming new network bytes (the caller can't see our buffer and
    // would otherwise block on the socket). Only reached when a record's
    // plaintext exceeded a partially-filled dst.
    let pending = with_engine(id, |s| std::mem::take(&mut s.plaintext_pending)).unwrap_or_default();
    if !pending.is_empty() {
        let mut idx = 0usize;
        for d in &dsts {
            if idx >= pending.len() {
                break;
            }
            // NOTE: a dst that accepts 0 bytes is FULL, not a stop signal —
            // keep scattering into the remaining buffers. See the identical
            // note on the Step-3 scatter below.
            idx += bb_write_from(ctx, *d, &pending[idx..]);
        }
        let hs = with_engine(id, |s| handshake_status_of(s)).unwrap_or(HS_NOT_HANDSHAKING_R);
        let status = if idx < pending.len() {
            with_engine(id, |s| {
                let mut rest = pending[idx..].to_vec();
                rest.extend_from_slice(&s.plaintext_pending);
                s.plaintext_pending = rest;
            });
            SR_BUFFER_OVERFLOW
        } else {
            SR_OK
        };
        if __dbg_hs {
            eprintln!(
                "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(pending-drain) status={} hs={} consumed=0 produced={}",
                std::thread::current().id(),
                id,
                status_name(status),
                hs_name(hs),
                idx
            );
        }
        let result = alloc_engine_result(ctx, status, hs, 0, idx as i32);
        return Ok(Some(Value::Object(Some(result?))));
    }

    // If the caller's dst has no room for APPLICATION data, do NOT
    // consume/decrypt records — they would be stuck in our buffer. Return
    // OVERFLOW so the caller drains its app buffer and retries (records stay in
    // src). Skipped while still handshaking: handshake records produce no app
    // plaintext, so a 0-capacity dst is normal and must not stall the handshake.
    let handshaking = with_engine(id, |s| {
        s.conn.as_ref().map(|c| c.is_handshaking()).unwrap_or(true)
    })
    .unwrap_or(true);
    if dst_cap == 0 && !handshaking {
        let hs = with_engine(id, |s| handshake_status_of(s)).unwrap_or(HS_NOT_HANDSHAKING_R);
        if __dbg_hs {
            eprintln!(
                "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(dst_cap==0) status=BUFFER_OVERFLOW hs={} consumed=0 produced=0",
                std::thread::current().id(),
                id,
                hs_name(hs)
            );
        }
        let result = alloc_engine_result(ctx, SR_BUFFER_OVERFLOW, hs, 0, 0);
        return Ok(Some(Value::Object(Some(result?))));
    }

    let src_view = bb_view(ctx, src);
    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_unwrap id={} SRC {}",
            std::thread::current().id(),
            id,
            bb_describe(ctx, src, &src_view)
        );
    }
    let (src_pos, src_lim) = (src_view.pos, src_view.lim);
    let mut offset = src_pos;

    // Not a TLS record at all (nio20 wave 3, L1; `NioConf19` `sslengine-garbage`).
    // `unwrap` is record-oriented, so the head of `src` is always a record
    // boundary; a head byte that is no TLS content type (20..=24) and no SSLv2
    // ClientHello (high bit set, message type 1 or 4 at offset 2) is what
    // JSSE's `SSLEngineInputRecord.bytesInCompletePacket` calls "gobblygook":
    // `SSLException("Unsupported or unrecognized SSL message")`, once five
    // bytes are present (fewer is still BUFFER_UNDERFLOW). The record loop
    // below read the bogus header's length instead (a plain-text
    // `GET / HTTP/1.1` claims 8239 bytes) and answered BUFFER_UNDERFLOW
    // forever, so a server never learned its peer was not speaking TLS.
    if !matches!(src_view.backing, BbBacking::Unresolved) && src_pos + 5 <= src_lim {
        let ct = bb_get_byte(ctx, &src_view, src_pos).unwrap_or(0);
        let b2 = bb_get_byte(ctx, &src_view, src_pos + 2).unwrap_or(0);
        let sslv2_hello = ct & 0x80 != 0 && (b2 == 1 || b2 == 4);
        if !(20..=24).contains(&ct) && !sslv2_hello {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLException",
                "Unsupported or unrecognized SSL message",
            ));
        }
    }

    // JSSE's server-side SNI gate, run at ClientHello time — see
    // `peek_client_hello_sni` for why it cannot wait until rustls has parsed
    // the record. Nothing is consumed here; on refusal the bytes are never fed
    // to rustls at all, so no ServerHello is ever produced.
    if let Some(host) = engine_pending_sni_host(ctx, id, &src_view, src_pos, src_lim) {
        with_engine(id, |s| s.requested_sni = Some(host.clone()));
        if engine_run_sni_match_check(ctx, id, this, host)? {
            // Refused. The engine now holds a fatal `unrecognized_name` for the
            // peer and a `deferred_handshake_error` for this side; this call
            // must report ORDINARY PROGRESS so the caller comes back for the
            // wrap that emits the alert, and only the wrap after that raises.
            //
            // Consuming the hello record is what makes it progress:
            // `SslHandler.decodeJdkCompatible` hands `unwrap` exactly one TLS
            // record and treats `bytesConsumed != packetLength` as "not an
            // SSL/TLS record" (`NotSslRecordException`), and a call that
            // consumed nothing reports BUFFER_UNDERFLOW — the caller then
            // waits for network data that is never coming and the deferred
            // failure is never drained. The bytes are dropped rather than fed
            // to rustls: a refused hello must not produce a ServerHello.
            let rec_end = {
                let b3 = bb_get_byte(ctx, &src_view, src_pos + 3).unwrap_or(0) as usize;
                let b4 = bb_get_byte(ctx, &src_view, src_pos + 4).unwrap_or(0) as usize;
                (src_pos + 5 + ((b3 << 8) | b4)).min(src_lim)
            };
            if rec_end > src_pos {
                bb_set_pos(ctx, src, src_view.layout, rec_end);
            }
            let consumed = rec_end.saturating_sub(src_pos);
            if __dbg_hs {
                eprintln!(
                    "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(sni-refused) status=OK hs=NEED_WRAP consumed={} produced=0",
                    std::thread::current().id(), id, consumed
                );
            }
            let result = alloc_engine_result(ctx, SR_OK, HS_NEED_WRAP_R, consumed as i32, 0)?;
            return Ok(Some(Value::Object(Some(result))));
        }
    }

    // Realize the rustls connection — and, on the first inbound handshake
    // flight, hand the caller a delegated task instead of processing it. This
    // is the ONLY place that defers; see `DelegatedTask`.
    //
    // Gated on a complete, plausible TLS record sitting at the head of `src`,
    // for two independent reasons:
    //
    //   * Deferring here CONSUMES the caller's records first, into
    //     `deferred_inbound`. netty's `SslHandler.decodeJdkCompatible` hands
    //     `unwrap` exactly one TLS record and treats
    //     `bytesConsumed != packetLength` as "not an SSL/TLS record": it
    //     throws `NotSslRecordException` and fails the handshake. With no
    //     record to consume there is nothing to report, so deferring would
    //     kill the handshake rather than delay it.
    //   * `src` may not hold a TLS record at all.
    //     `SSLEngineTest.testSSLEngineUnwrapNoSslRecord` feeds a zeroed
    //     application buffer and requires an `SSLException`; staging it as
    //     3277 five-byte "records" would answer NEED_TASK instead. Anything
    //     that does not look like a record falls through to rustls, which is
    //     what raises the error.
    //
    // The lock is dropped before any failure is reported — see the identical
    // note in `do_wrap`.
    let head_is_tls_record = {
        let mut ok = false;
        if !matches!(src_view.backing, BbBacking::Unresolved) && src_pos + 5 <= src_lim {
            let ct = bb_get_byte(ctx, &src_view, src_pos).unwrap_or(0);
            let vmaj = bb_get_byte(ctx, &src_view, src_pos + 1).unwrap_or(0);
            let b3 = bb_get_byte(ctx, &src_view, src_pos + 3).unwrap_or(0) as usize;
            let b4 = bb_get_byte(ctx, &src_view, src_pos + 4).unwrap_or(0) as usize;
            // ChangeCipherSpec / Alert / Handshake / ApplicationData, TLS
            // version major 3, and the whole record present.
            ok = (20..=23).contains(&ct) && vmaj == 3 && src_pos + 5 + ((b3 << 8) | b4) <= src_lim;
        }
        ok
    };
    let defer_decision = if head_is_tls_record {
        engine_begin_or_defer(id)
    } else {
        // Nothing stageable: realize the connection the way `do_wrap` does and
        // let the record loop (or rustls's error) speak.
        with_engine(id, |s| engine_begin_if_needed(s).map(|()| false)).unwrap_or(Ok(false))
    };
    match defer_decision {
        Err(e) => {
            if __dbg_hs {
                eprintln!(
                    "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(engine_begin ERROR) err={}",
                    std::thread::current().id(),
                    id,
                    e
                );
            }
            return Err(engine_begin_failure(ctx, e));
        }
        Ok(true) => {
            let mut staged: Vec<u8> = Vec::new();
            if !matches!(src_view.backing, BbBacking::Unresolved) {
                // Whole records only, exactly as the main loop below decides
                // it: an incomplete record stays in the caller's buffer for
                // its next unwrap.
                while offset + 5 <= src_lim {
                    let b3 = bb_get_byte(ctx, &src_view, offset + 3).unwrap_or(0) as usize;
                    let b4 = bb_get_byte(ctx, &src_view, offset + 4).unwrap_or(0) as usize;
                    let rec_end = offset + 5 + ((b3 << 8) | b4);
                    if rec_end > src_lim {
                        break;
                    }
                    let rec = bb_bytes_range(ctx, &src_view, offset, rec_end);
                    if rec.len() != rec_end - offset {
                        // Backing could not produce the whole record (clamped
                        // direct access) — leave it where it is.
                        break;
                    }
                    staged.extend_from_slice(&rec);
                    offset = rec_end;
                }
            }
            let consumed = offset - src_pos;
            if consumed > 0 {
                bb_set_pos(ctx, src, src_view.layout, offset);
                with_engine(id, |s| s.deferred_inbound.extend_from_slice(&staged));
            }
            if __dbg_hs {
                eprintln!(
                    "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(delegated-task) status=OK hs=NEED_TASK consumed={} produced=0",
                    std::thread::current().id(), id, consumed
                );
            }
            let result = alloc_engine_result(ctx, SR_OK, HS_NEED_TASK_R, consumed as i32, 0)?;
            return Ok(Some(Value::Object(Some(result))));
        }
        Ok(false) => {}
    }

    // These five span all three phases below, so they live outside every lock.
    let mut plaintext: Vec<u8> = Vec::new();
    let mut underflow = false;
    // Collected inside the record loop (which holds the connection mutably) and
    // written back to the engine once the loan is returned.
    let mut captured_session_id: Option<Vec<u8>> = None;
    // The loop stopped because the caller's destination could not hold the
    // next record's plaintext — BUFFER_OVERFLOW, not BUFFER_UNDERFLOW. The
    // two are opposite instructions ("give me a bigger buffer" vs "read more
    // network data"), and answering UNDERFLOW when nothing more is coming is
    // how a caller spins.
    let mut dst_too_small = false;
    let mut deferred_error: Option<(&'static str, String)> = None;

    // ---- PHASE 1 (registry LOCKED): the deferred-task replay. --------------
    //
    // Split out from the record loop because the loop must run with the lock
    // DROPPED — see `ConnCheckout`. This phase touches `s` and cannot.
    {
        let cell = match engine_cell(id) {
            Some(cell) => cell,
            None => {
                if __dbg_hs {
                    eprintln!(
                        "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(engine-handle-missing ERROR)",
                        std::thread::current().id(),
                        id
                    );
                }
                return Err(RuntimeError::IOException {
                    message: "engine handle missing".into(),
                }
                .into());
            }
        };
        // The failure is carried OUT of this block and raised once the engine
        // lock is dropped (gc-common w11-a): `throw_jca_exc` allocates, and
        // allocating under an engine lock is the hazard
        // `EngineState::trust_managers_ctx_key` documents.
        let replay_failure: Option<(&'static str, String)> = {
            let mut g = cell.lock();
            let s: &mut EngineState = &mut g;
            // Replay whatever a delegated task deferred. These records were taken
            // out of the caller's buffer before the connection existed (see the
            // NEED_TASK arm above), so the caller will never present them again —
            // this is the only place left that can feed them to rustls.
            //
            // A failure here is classified exactly as the record loop below
            // classifies its own, because it IS the same failure, merely one call
            // later: the error KIND picks the `SSLException` subclass, and a
            // server defers it so rustls's fatal alert reaches the peer first.
            let staged = std::mem::take(&mut s.deferred_inbound);
            if !staged.is_empty() {
                match engine_unwrap_pump_raw(s, &staged) {
                    Ok((_, pt)) => {
                        plaintext.extend_from_slice(&pt);
                        None
                    }
                    Err(e) => {
                        let handshaking_now = s.conn.as_ref().is_some_and(|c| c.is_handshaking());
                        if handshaking_now {
                            // Raised here, NOT deferred the way the record loop
                            // below defers a server's handshake error.
                            //
                            // That deferral works only because the loop consumed a
                            // record: the call reports progress, the caller comes
                            // back, and the `wrap` that drains rustls's fatal
                            // alert raises the error on its way out. This replay
                            // consumed nothing from `src` — its records were taken
                            // a call earlier, by the NEED_TASK arm — so the call
                            // reports BUFFER_UNDERFLOW, the caller waits for
                            // network data that is never coming, and the deferred
                            // error is never drained. Measured with the deferral:
                            // `testHandshakeFailureCipherMissmatch{TLSv12,TLSv13}Jdk`
                            // fail with `StacklessClosedChannelException`, which is
                            // the exact symptom that deferral was introduced to
                            // cure. Both pass when it is raised here.
                            let cls = jsse_handshake_exception_class(&e);
                            Some((cls, handshake_error_message(&e)))
                        } else {
                            let (cls, msg) = match e {
                                rustls::Error::AlertReceived(desc) => (
                                    jsse_received_alert_exception_class(desc),
                                    jsse_received_alert_message(desc),
                                ),
                                other => (
                                    "javax/net/ssl/SSLException",
                                    format!("rustls process_new_packets: {other}"),
                                ),
                            };
                            Some((cls, msg))
                        }
                    }
                }
            } else {
                None
            }
        };
        if let Some((cls, msg)) = replay_failure {
            return Err(crate::phases_early::throw_jca_exc(ctx, cls, &msg));
        }
    }
    // ---- PHASE 2 (registry UNLOCKED, connection ON LOAN): the record loop. --
    //
    // The lock is dropped and the connection checked out for exactly this
    // stretch, because `process_new_packets` below calls
    // `PassthroughServerCertVerifier::verify_server_cert`, which consults the
    // application's Java `TrustManager`. That upcall may call back into an
    // engine native, and the registry's write guard is not reentrant.
    //
    // `ctx` is published for the same window so the verifier can reborrow it —
    // the mechanism `JavaKeyManagerResolver::resolve` already uses from inside
    // this same `process_new_packets`.
    //
    // The loop body is unchanged: a census of `s.*` accesses between its braces
    // finds none. Its early `return Err(...)` exits are safe because
    // `ConnCheckout::drop` restores the connection on every path.
    let src_resolved = !matches!(src_view.backing, BbBacking::Unresolved);
    {
        let mut checkout = ConnCheckout::take(id);
        // nio20 wave 4 (L4): see `SinkRecordingClientCertResolver`. Read after
        // the checkout released the engine's lock, held for the record loop.
        let _presented_sink =
            PresentedSink::enter(with_engine(id, |s| s.client_cert_presented.clone()).flatten());
        let _active_ctx = set_active_native_context(ctx);
        // Pinned for the window and released with it — see
        // `set_active_engine_binding`. `unpin_native_roots(pin)` releases this
        // frame and anything a nested `engine_run_trust_check` took above it.
        let (_active_engine, engine_pin) = set_active_engine_binding(ctx, id, this);
        let _unpin = UnpinOnDrop { base: engine_pin };
        if let (true, Some(conn)) = (src_resolved, checkout.conn.as_mut()) {
            loop {
                if offset >= src_lim {
                    break;
                }
                if offset + 5 > src_lim {
                    underflow = true; // incomplete record header
                    break;
                }
                let b3 = bb_get_byte(ctx, &src_view, offset + 3).unwrap_or(0) as usize;
                let b4 = bb_get_byte(ctx, &src_view, offset + 4).unwrap_or(0) as usize;
                let rec_len = (b3 << 8) | b4;
                let rec_end = offset + 5 + rec_len;
                if rec_end > src_lim {
                    underflow = true; // incomplete record body
                    break;
                }
                // Don't START a record whose plaintext could overflow the dst.
                //
                // The `!plaintext.is_empty()` guard used to make this apply only
                // from the SECOND record on, so the FIRST record of a call was
                // always decrypted no matter how small the caller's buffer was:
                // the excess went to `plaintext_pending` and the call reported
                // BUFFER_OVERFLOW having already consumed the record. JSSE
                // consumes NOTHING in that case, and
                // `SSLEngineTest.testUnwrapBehavior` asserts exactly that
                // (`assertEquals(remaining, encryptedClientToServer.remaining())`
                // after an unwrap into a 3-byte buffer).
                //
                // `rec_len - 17` is a safe upper bound on a record's plaintext
                // for every suite this engine negotiates: TLS 1.3 spends at
                // least 1 byte of content type plus a 16-byte AEAD tag, TLS 1.2
                // AEAD spends 8 + 16, and CBC spends more still. Using the
                // bound rather than the exact length keeps the decision on the
                // "do not feed rustls" side of the record, which is the only
                // side from which nothing has been consumed yet.
                //
                // The gate applies only once the handshake is over: handshake
                // records carry no application plaintext, and refusing to feed
                // one for want of application-buffer space would wedge the
                // handshake itself.
                //
                // The bound is ALSO capped at `getApplicationBufferSize()`
                // (16384, the RFC 8446 §5.1 TLSPlaintext limit this engine
                // reports): a caller that sized its buffer the documented way
                // must never be told a record does not fit. Without the cap a
                // full-size TLS 1.2 AEAD record (8-byte explicit nonce + 16-byte
                // tag) bounds to 16391 against a 16384-byte buffer and every
                // retry answers BUFFER_OVERFLOW again — a livelock, not a
                // stricter contract.
                let max_plaintext = rec_len.saturating_sub(17).min(16384);
                if !handshaking && plaintext.len() + max_plaintext > dst_cap {
                    dst_too_small = true;
                    break;
                }
                // One application record per `unwrap`, as JSSE does. Decrypting
                // every record that fits made a single call answer with two
                // records' plaintext at once, so a caller that checks
                // `bytesProduced()` per record saw the sum
                // (`testUnwrapBehavior` expects 5 then 6, and got 11 then 0).
                // Handshake records produce no plaintext and still batch, so
                // the handshake driver's iteration count is unchanged.
                if !plaintext.is_empty() {
                    break;
                }
                let rec_total = 5 + rec_len;
                let rec = bb_bytes_range(ctx, &src_view, offset, rec_end);
                // The CLIENT side of the shared TLS 1.2 session id — the mirror
                // of the server's capture in `do_wrap`. Reads the record before
                // rustls consumes it; see `peek_server_hello_session_id`.
                if captured_session_id.is_none() {
                    captured_session_id = peek_server_hello_session_id(&rec);
                }
                if rec.len() != rec_total {
                    // Backing couldn't produce the full record (clamped
                    // direct access) — treat like an incomplete record.
                    underflow = true;
                    break;
                }
                // Whether the handshake is still in progress BEFORE this record
                // is fed — used below to stop at the handshake-completion
                // boundary exactly like real `SSLEngineImpl.unwrap` does.
                let was_handshaking = conn.is_handshaking();
                // Feed the ENTIRE record into rustls. `read_tls` reads only as
                // much as its deframer buffer takes per call (often less than a
                // full 16 KiB record), so loop until the cursor is drained —
                // otherwise rustls holds a partial record, decrypts nothing
                // (`produced=0`), and the request body never reaches the servlet.
                let mut cur = std::io::Cursor::new(rec);
                let mut fed_ok = true;
                while (cur.position() as usize) < rec_total {
                    match conn.read_tls(&mut cur) {
                        Ok(0) => break,
                        Ok(_) => {}
                        Err(_) => {
                            fed_ok = false;
                            break;
                        }
                    }
                }
                if !fed_ok {
                    break;
                }
                let __pnp_result = conn.process_new_packets();
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!(
                        "[dbg-tls-auth] do_unwrap id={} process_new_packets -> {} is_handshaking={} peer_certs_present={}",
                        id,
                        if __pnp_result.is_ok() { "Ok".to_string() } else { format!("Err({:?})", __pnp_result.as_ref().err()) },
                        conn.is_handshaking(),
                        conn.peer_certificates().map(|c| c.len()).unwrap_or(0)
                    );
                }
                if let Err(e) = __pnp_result {
                    // A rustls protocol error surfacing while the handshake is
                    // still in progress (e.g. `InvalidCertificate` — the peer's
                    // certificate failed validation against the trust store)
                    // must reach Java as `SSLHandshakeException`, not a bare
                    // `IOException`. Real JSSE's `SSLEngine.unwrap()` throws
                    // `SSLHandshakeException` for essentially any failure
                    // during the handshake phase; callers (e.g. Apache
                    // httpasyncclient's `SSLIOSession`) that specifically
                    // catch `SSLException`/`SSLHandshakeException` to
                    // distinguish a TLS failure from an ordinary I/O error
                    // never see it with a plain `IOException`, and the
                    // connection teardown that follows can surface as an
                    // unrelated `ConnectionClosedException` instead
                    // (RestClientBuilderIntegTests.testBuilderUsesDefaultSSLContext).
                    if __dbg_hs {
                        eprintln!(
                            "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(process_new_packets ERROR) is_handshaking={} err={:?}",
                            std::thread::current().id(),
                            id,
                            conn.is_handshaking(),
                            e
                        );
                    }
                    if conn.is_handshaking() {
                        // rustls has already queued the fatal TLS alert. On a
                        // server engine, let the handshake driver observe NEED_WRAP
                        // and flush it before the channel closes; otherwise Netty
                        // reports only ClosedChannelException to the client.
                        //
                        // A CLIENT does NOT defer, and that was MEASURED rather than
                        // assumed. Deferring its own `TrustManager` rejection so the
                        // next `wrap` could drain the alert first looked right — it is
                        // the shape the server branch uses — and it wedged the client
                        // instead: netty never issued that wrap, so the deferred error
                        // never drained and `handshakeFuture().await()` never returned.
                        // `testHandshakeFailureOnlyFireExceptionOnce` went from ~50%
                        // to a 10 s `@Timeout` on every run, failing at line 1545 (the
                        // CLIENT's assertion) instead of 1546 (the server's). The
                        // NEED_WRAP-while-pending rule can only be relied on where the
                        // caller is already in a wrap-driving state.
                        if matches!(&*conn, EngineConn::Server(_)) {
                            // Deferred, not discarded: the next `wrap` drains
                            // the alert and then raises this.
                            deferred_error = Some((
                                jsse_handshake_exception_class(&e),
                                handshake_error_message(&e),
                            ));
                            offset = rec_end;
                            break;
                        }
                        let cls = jsse_handshake_exception_class(&e);
                        let msg = handshake_error_message(&e);
                        return Err(crate::phases_early::throw_jca_exc(ctx, cls, &msg));
                    }
                    // POST-handshake record-layer failure. `SSLException`, not
                    // a bare `IOException`, for the same reason the handshake
                    // branch above gives — and here it is load-bearing rather
                    // than merely tidy: a **fatal alert from the peer** lands
                    // on this line, and it is the only signal that says "the
                    // other side rejected us" as opposed to "the socket
                    // dropped". netty's
                    // `ParameterizedSslHandlerTest.testAlertProducedAndSend`
                    // waits for exactly `cause.getCause() instanceof
                    // SSLException` and hung forever (~170x HotSpot's 6 s) on
                    // the `IOException` this used to throw. `SSLException`
                    // extends `IOException`, so every existing
                    // `catch (IOException)` is unaffected.
                    let (cls, msg) = match e {
                        rustls::Error::AlertReceived(desc) => (
                            // nio20 wave 4 fix-back (L4): the class JSSE's
                            // alert consumer picks, not always `SSLException`.
                            jsse_received_alert_exception_class(desc),
                            // JSSE's wording, so a caller matching on the
                            // message sees what it sees on HotSpot.
                            jsse_received_alert_message(desc),
                        ),
                        other => (
                            "javax/net/ssl/SSLException",
                            format!("rustls process_new_packets: {other}"),
                        ),
                    };
                    return Err(crate::phases_early::throw_jca_exc(ctx, cls, &msg));
                }
                let mut tmp = [0u8; 16384];
                loop {
                    match conn.reader().read(&mut tmp) {
                        Ok(0) => break,
                        Ok(n) => plaintext.extend_from_slice(&tmp[..n]),
                        Err(_) => break,
                    }
                }
                offset = rec_end;
                // FIX (handshake-boundary): stop consuming records the moment
                // the handshake COMPLETES, leaving any already-arrived
                // application-data records in `src` for the caller's next
                // unwrap. Real `SSLEngineImpl.unwrap` never crosses this
                // boundary in one call, and drivers depend on that:
                // `sun.net.httpserver.SSLStreams.doHandshake`'s NEED_UNWRAP
                // branch unwraps into a THROWAWAY scratch buffer, so when the
                // client's Finished and its first request bytes arrive in one
                // TCP read (a pure timing race — reproduced ~50% on TlsRepro3),
                // a greedy unwrap that consumed both returned hs=FINISHED with
                // the decrypted HTTP request in a buffer the driver discards.
                // The server then blocked reading a request the client had
                // already sent, and the client timed out ("Read timed out").
                if was_handshaking && !conn.is_handshaking() {
                    break;
                }
                if plaintext.len() >= dst_cap {
                    break;
                }
            }
        }
    } // <- ConnCheckout drops here: the connection is back in the registry and
      //    `conn_checked_out` is clear, so PHASE 3 sees a normal engine.

    // ---- PHASE 3 (registry LOCKED): write back and classify. ---------------
    let (status, hs, pending_trust_check) = {
        let cell = match engine_cell(id) {
            Some(cell) => cell,
            None => {
                return Err(RuntimeError::IOException {
                    message: "engine handle missing".into(),
                }
                .into());
            }
        };
        let mut g = cell.lock();
        let s: &mut EngineState = &mut g;
        if let Some(sid) = captured_session_id {
            if s.negotiated_session_id.is_empty() {
                s.negotiated_session_id = sid;
            }
        }
        if deferred_error.is_some() {
            s.deferred_handshake_error = deferred_error;
        }
        engine_capture_negotiation(s);
        let _ = underflow;
        let mut status = SR_OK;
        if plaintext.is_empty() && offset == src_pos {
            // No progress. Which of the two "try again" answers depends on WHY:
            // a destination too small for the next record's plaintext is
            // BUFFER_OVERFLOW (`SSLEngineTest.testUnwrapBehavior` unwraps into a
            // 3-byte buffer and asserts exactly that, with `src` untouched);
            // anything else is src being empty or holding a partial record, i.e.
            // BUFFER_UNDERFLOW (returning OK there makes Tomcat's handshake loop
            // spin forever).
            status = if dst_too_small {
                SR_BUFFER_OVERFLOW
            } else {
                SR_BUFFER_UNDERFLOW
            };
        }
        // The peer's `close_notify` has arrived: JSSE reports CLOSED from this
        // unwrap and every one after it, and that report is the ONLY way a
        // caller learns the connection was closed cleanly rather than dropped.
        //
        // netty's `SslHandler.unwrap` switches on exactly this
        // (`case CLOSED: notifyClosure = true`) to fire
        // `SslCloseCompletionEvent`; without it `CloseNotifyTest` sees the
        // decrypted response arrive and then no close event at all. Marking
        // `closed_inbound` here is the same fact seen through
        // `isInboundDone()`, which is how a caller that polls rather than
        // switches finds out.
        //
        // CLOSED overrides BUFFER_UNDERFLOW deliberately: once the peer has
        // closed there is no more network data to ask for, and telling the
        // caller to read more is how a close turns into a spin.
        if s.conn.as_ref().is_some_and(|c| c.peer_has_closed()) {
            s.closed_inbound = true;
            status = SR_CLOSED;
            // TLS 1.2 and below: answer the peer's `close_notify` with our
            // own, automatically, the way JSSE does.
            //
            // RFC 5246 §7.2.1 makes the response required; RFC 8446 §6.1 makes
            // it optional, and JSSE's TLS 1.3 engine does NOT send one — an
            // asymmetry netty encodes directly (`CloseNotifyTest.jdkTls13`
            // takes a different branch for exactly this, and asserts the
            // automatic response on every other parameterisation). rustls
            // queues nothing on its own in either case, so under TLS 1.2 the
            // peer waited for a record that was never coming:
            // `ParameterizedSslHandlerTest.testCloseNotify`'s client promise
            // never completed.
            //
            // Marking the engine outbound-closed as well is what JSSE does
            // here too — an automatic close is a full close, and the next
            // `wrap` is the one that emits the record (see `do_wrap`'s
            // closed-outbound drain).
            let responds = s
                .conn
                .as_ref()
                .and_then(|c| c.protocol_version())
                .is_some_and(|v| v != rustls::ProtocolVersion::TLSv1_3);
            if responds && !s.closed_outbound {
                s.closed_outbound = true;
                if let Some(c) = s.conn.as_mut() {
                    c.send_close_notify();
                }
            }
        }
        let hs = handshake_status_of(s);
        if hs == HS_FINISHED_R {
            s.handshake_finished_reported = true;
        }
        // Extract-only — see `engine_take_pending_trust_check`'s doc for why
        // the actual Java call must happen after this lock is dropped.
        // A verifier-time consultation has already set `trust_check_done`, so
        // this answers `None` on its own — see `mark_trust_check_done`.
        let pending_trust_check = engine_take_pending_trust_check(id, s);
        (status, hs, pending_trust_check)
    };
    // `engine_run_trust_check` runs the application's TrustManager — arbitrary
    // Java that allocates. `this`, `src` and EVERY element of `dsts` are raw
    // `ObjectRef`s held across it, and `src` and the `dsts` are WRITTEN THROUGH
    // below. Pin them all and re-derive after the callback; the `dsts` vector
    // is the worse half, because a collection leaves every element stale rather
    // than one. See the retired `unpinned-native-locals-audit` write-up.
    let mut this = this;
    let mut src = src;
    if let Some(pending) = pending_trust_check {
        this = ctx.read_native_pin(this_entry_pin, this);
        engine_run_trust_check(ctx, pending, Some(this))?;
    }
    let _ = this;
    let consumed = offset - src_pos;
    src = ctx.read_native_pin(src_entry_pin, src);
    bb_set_pos(ctx, src, src_view.layout, offset);

    // Step 3: write plaintext into dsts (may span multiple buffers).
    //
    // A dst that accepts 0 bytes is simply FULL — it must NOT stop the scatter,
    // because `unwrap(src, dsts, off, len)` is a scattering operation and later
    // buffers may still have room. Tomcat's HTTP/2 async parser reads every
    // frame into `[frameHeader(9), framePayload(maxFrameSize)]`; once the 9-byte
    // header is filled by the first unwrap, EVERY subsequent unwrap sees dst[0]
    // full. Breaking there produced `produced=0` while the record had already
    // been consumed, so the whole record went to `plaintext_pending`, which the
    // caller cannot see. The connection then wedged in a BUFFER_OVERFLOW loop
    // and the request body was truncated after the first DATA frame
    // (`TestLargeUpload`: 13107 of 65535 bytes read by the servlet).
    let mut produced_total = 0usize;
    let mut idx = 0usize;
    for (i, dst) in dsts.iter().enumerate() {
        if idx >= plaintext.len() {
            break;
        }
        // Each `bb_write_from` runs Java of its own, so the NEXT buffer in the
        // scatter has to be re-derived rather than read from the vector.
        let dst_now = ctx.read_native_pin(dst_entry_pins[i], *dst);
        let n = bb_write_from(ctx, dst_now, &plaintext[idx..]);
        produced_total += n;
        idx += n;
    }
    // If we have plaintext left over, signal BUFFER_OVERFLOW and stash the
    // remainder back in rustls' reader by re-injecting via writer? We can't —
    // rustls drained. Instead, we have to keep it in a local cache. For our
    // surface, plaintext leftover indicates the caller's dst was too small;
    // we surface that via BUFFER_OVERFLOW. The caller must enlarge dst.
    let final_status = if idx < plaintext.len() {
        // The caller's dst buffers were too small for all the decrypted bytes.
        // Stash the remainder in the DEDICATED plaintext-pending buffer (NOT
        // `outbound`, which holds encrypted TLS records destined for `wrap` —
        // mixing plaintext there makes the next `wrap` emit it on the wire and
        // the peer aborts with "corrupt message"). Tomcat's SecureNioChannel
        // responds to BUFFER_OVERFLOW by enlarging the app buffer and
        // re-unwrapping, which drains `plaintext_pending` on the retry.
        with_engine(id, |s| {
            s.plaintext_pending.extend_from_slice(&plaintext[idx..]);
        });
        SR_BUFFER_OVERFLOW
    } else {
        status
    };

    if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} do_unwrap id={} RETURN(normal) status={} hs={} consumed={} produced={}",
            std::thread::current().id(),
            id,
            status_name(final_status),
            hs_name(hs),
            consumed,
            produced_total
        );
    }
    let result = alloc_engine_result(
        ctx,
        final_status,
        hs,
        consumed as i32,
        produced_total as i32,
    );
    Ok(Some(Value::Object(Some(result?))))
}

// -----------------------------------------------------------------------------
// ALPN registrations on SSLParameters / SSLEngineImpl + SNI hook on
// sun.security.ssl.SSLContextImpl
// -----------------------------------------------------------------------------

pub fn register_alpn_on_parameters(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLParameters";

    // setApplicationProtocols stores into a side-table keyed by SSLParameters
    // ObjectRef. Mirrored when applySSLParameters is called.
    r.register(
        cls,
        "setApplicationProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // `SSLParameters.setApplicationProtocols` VALIDATES, and the two
            // checks are the ones an ALPN caller most needs: a null array and
            // a null-or-empty element. MEASURED on HotSpot 25.0.4+7
            // (`L6TlsParamSweep` rows 30, 31, 33) — this engine accepted all
            // three, so `new String[]{"h2", null}` became an advertised
            // protocol list with a hole in it and the failure surfaced at
            // handshake time, on the wire, in another process.
            let Some(Value::Object(Some(arr))) = args.get(1) else {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "protocols was null".into(),
                }
                .into());
            };
            let arr = *arr;
            let len = ctx.array_length(arr);
            let mut list: Vec<String> = Vec::with_capacity(len);
            for i in 0..len {
                let element = match ctx.get_array_element(arr, i) {
                    Value::Object(Some(s)) => ctx.read_string(s),
                    _ => None,
                };
                match element {
                    Some(text) if !text.is_empty() => list.push(text),
                    _ => {
                        return Err(RuntimeError::IllegalArgumentException {
                            message: "An element of protocols was null/empty".into(),
                        }
                        .into())
                    }
                }
            }
            // Key computed before the guard (gc-common w11-a): it calls
            // `ctx.identity_hash_code`.
            let key = engine_objref_key(ctx, this);
            sslparams_alpn_table().lock().insert(key, list);
            // gc-common w12-a: the row lives as long as this `SSLParameters`
            // (nothing above allocates, so `this` is current).
            note_tls_weak_owner(ctx, TlsOwnerKind::SslParams, key, this);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        cls,
        "getApplicationProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Key computed before the guard (gc-common w11-a). A lookup: a
            // parameters object nobody configured is never keyed (gc-common
            // w26-b), so a `getSSLParameters()` copy leaves no slot behind.
            let key = existing_objref_key(ctx, this);
            let list = key
                .and_then(|key| sslparams_alpn_table().lock().get(&key).cloned())
                // A fresh `SSLParameters` advertises NOTHING: HotSpot's
                // `getApplicationProtocols()` on one nobody has configured is
                // a zero-length array, not this VM's invented `[h2,
                // http/1.1]`. A caller that reads the list to decide whether
                // ALPN was requested was told yes by every parameters object
                // in the VM.
                .unwrap_or_default();
            // Typed and rooted — see `materialize_java_string_array`.
            let arr = materialize_java_string_array(ctx, &list);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn register_apply_parameters(r: &mut NativeMethodRegistry) {
    // SSLEngine.setSSLParameters propagates the ALPN list onto the engine.
    r.register(
        "sun/security/ssl/SSLEngineImpl",
        "setSSLParameters",
        "(Ljavax/net/ssl/SSLParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            if let Some(Value::Object(Some(p))) = args.get(1) {
                // Key computed before the guard (gc-common w11-a); a lookup,
                // so it never mints (gc-common w26-b).
                let p_key = existing_objref_key(ctx, *p);
                let alpn = p_key.and_then(|k| sslparams_alpn_table().lock().get(&k).cloned());
                if let Some(list) = alpn {
                    with_engine(id, |s| {
                        s.alpn_protocols = list.into_iter().map(|s| s.into_bytes()).collect();
                    });
                }
                // Apache HttpComponents 5 (and Tomcat's NioEndpoint, for its
                // ALPN/client-auth-capable connectors) configure TLS options
                // via an `SSLParameters` object passed to
                // `SSLEngine.setSSLParameters()`, not the legacy
                // `setEnabledCipherSuites`/`setEnabledProtocols` setters (see
                // `setEnabledCipherSuites` above, and the analogous fix for
                // the SSLSocket path in `phases_late.rs`'s
                // `stash_pending_layered_socket`/`setSSLParameters`/
                // `setEnabledCipherSuites` registrations). Without this, a
                // cipher-suite restriction set this way was silently
                // dropped: the engine kept its full default cipher list, so
                // a deliberately-mismatched client/server cipher
                // configuration (`connectWithSslBundleAndOptionsMismatch`)
                // still found a common cipher and the handshake succeeded
                // instead of failing with `SSLHandshakeException` as
                // real-JDK does.
                //
                // gc-common w20-e: every `SSLParameters` getter below runs
                // Java, and the parameters (read from `args`, a copy no
                // collection updates), the engine, the server-name list and
                // each server name used to be used raw after one. All are
                // pinned now and re-read after each call; the engine was
                // handed to `capture_sni_matchers`, which keys and weak-owns
                // its row by that address, at its entry address.
                let this_pin = ctx.pin_native_root(this);
                let p_pin = ctx.pin_native_root(*p);
                if let Ok(Some(Value::Object(Some(arr)))) =
                    ctx.invoke_virtual(*p, "getCipherSuites", "()[Ljava/lang/String;", &[])
                {
                    let len = ctx.array_length(arr);
                    let mut ciphers = Vec::with_capacity(len);
                    for i in 0..len {
                        if let Value::Object(Some(s)) = ctx.get_array_element(arr, i) {
                            if let Some(name) = ctx.read_string(s) {
                                ciphers.push(name);
                            }
                        }
                    }
                    if !ciphers.is_empty() {
                        with_engine(id, |s| {
                            s.enabled_ciphers = ciphers;
                        });
                    }
                }
                // Tomcat configures client-cert auth via
                // `SSLParameters.setNeed/WantClientAuth` + `engine.setSSLParameters`,
                // NOT the engine's own setNeed/WantClientAuth. Read those booleans
                // off the SSLParameters object and apply them, else the server
                // never requests the client cert and mTLS auth returns HTTP 401.
                let params = ctx.read_native_pin(p_pin, *p);
                let need = ctx
                    .get_field_by_name(params, "needClientAuth")
                    .as_int()
                    .unwrap_or(0)
                    != 0;
                let want = ctx
                    .get_field_by_name(params, "wantClientAuth")
                    .as_int()
                    .unwrap_or(0)
                    != 0;
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!(
                        "[dbg-tls-auth] setSSLParameters id={} need={} want={}",
                        id, need, want
                    );
                }
                with_engine(id, |s| {
                    if need {
                        s.need_client_auth = true;
                        s.want_client_auth = false;
                    } else if want {
                        s.want_client_auth = true;
                        s.need_client_auth = false;
                    }
                });
                // Endpoint identification — the JSSE switch that turns RFC 2818
                // hostname verification on. Tomcat's WebSocket client sets it
                // exactly this way (`WsWebSocketContainer.createSSLEngine`:
                // `sslParams.setEndpointIdentificationAlgorithm("HTTPS")` then
                // `engine.setSSLParameters(sslParams)`) — it IS the fix for
                // CVE-2018-8034. Dropping it here meant the engine never
                // checked the peer's certificate against the host being
                // dialled, so a `localhost`-only certificate was accepted for
                // `127.0.0.1` (`TestSecurity2018.testCVE_2018_8034`).
                //
                // Read through the accessor rather than a field slot: in real-
                // JDK mode this is a genuine `javax.net.ssl.SSLParameters` whose
                // `identificationAlgorithm` field we must not address by index,
                // and in synthetic mode `tls.rs`'s native getter answers from
                // its own 6-field layout. Both spell the value the same way.
                let alg = match ctx.invoke_virtual(
                    params,
                    "getEndpointIdentificationAlgorithm",
                    "()Ljava/lang/String;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s),
                    _ => None,
                };
                // An explicit empty string / null CLEARS the algorithm — JSSE
                // treats both as "no endpoint identification" — so mirror the
                // caller's value exactly instead of only ever setting it.
                let alg = alg.filter(|a| !a.trim().is_empty());
                with_engine(id, |s| {
                    s.endpoint_id_alg = alg.clone();
                });
                // `setServerNames` — the CLIENT-side counterpart of the matcher
                // gate: the name this connection is for, overriding the host it
                // was dialled on.
                //
                // JSSE uses it for BOTH the `server_name` extension AND the
                // endpoint-identity check (`X509TrustManagerImpl.checkIdentity`
                // prefers the requested SNI host name over the socket's peer
                // host). Dropping it meant a client that dialled an IP and
                // declared an SNI name verified the certificate against the IP:
                // netty's `testUsingX509TrustManagerVerifiesSNIHostname`
                // connects to `127.0.0.1` with `serverName(new SNIHostName(
                // "something.netty.io"))` against a certificate for that name
                // and expects success; it got `endpoint identification (HTTPS)
                // failed for host "127.0.0.1"`.
                let params = ctx.read_native_pin(p_pin, *p);
                if let Ok(Some(Value::Object(Some(list)))) =
                    ctx.invoke_virtual(params, "getServerNames", "()Ljava/util/List;", &[])
                {
                    with_engine(id, |st| st.explicit_sni = Some(None));
                    let list_pin = ctx.pin_native_root(list);
                    let n = ctx
                        .invoke_virtual(list, "size", "()I", &[])
                        .ok()
                        .flatten()
                        .and_then(|v| v.as_int())
                        .unwrap_or(0);
                    for i in 0..n.min(16) {
                        let list_now = ctx.read_native_pin(list_pin, list);
                        let Ok(Some(Value::Object(Some(sn)))) = ctx.invoke_virtual(
                            list_now,
                            "get",
                            "(I)Ljava/lang/Object;",
                            &[Value::Int(i)],
                        ) else {
                            break;
                        };
                        let sn_pin = ctx.pin_native_root(sn);
                        // Only `SNI_HOST_NAME` (type 0) names a host.
                        let is_host_name = matches!(
                            ctx.invoke_virtual(sn, "getType", "()I", &[]),
                            Ok(Some(Value::Int(0)))
                        );
                        let sn_now = ctx.read_native_pin(sn_pin, sn);
                        ctx.unpin_native_roots(sn_pin);
                        if !is_host_name {
                            continue;
                        }
                        if let Ok(Some(Value::Object(Some(s)))) =
                            ctx.invoke_virtual(sn_now, "getAsciiName", "()Ljava/lang/String;", &[])
                        {
                            if let Some(host) = ctx.read_string(s) {
                                if !host.is_empty() {
                                    with_engine(id, |st| {
                                        st.explicit_sni = Some(Some(host.clone()));
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
                // SNI matchers — the server-side gate. See
                // `engine_run_sni_match_check`.
                let engine = ctx.read_native_pin(this_pin, this);
                let params = ctx.read_native_pin(p_pin, *p);
                ctx.unpin_native_roots(this_pin);
                capture_sni_matchers(ctx, engine, params);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    r.register(
        "sun/security/ssl/SSLEngineImpl",
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(ctx, this);
            // BUG-08: the prior bare `alloc_concurrent_synthetic` SSLParameters left
            // the REAL `protocols`/`cipherSuites` fields null, so the un-intercepted
            // `SSLParameters.getProtocols()`/`getCipherSuites()` bytecode returned
            // null. Jetty's `SslContextFactory.checkConfiguration()` does
            // `engine.getSSLParameters().getProtocols()` then reads its array length
            // → NPE ("Cannot read the array length because <local2> is null", 5×
            // JettyClientHttpRequestFactoryTests). Build a REAL SSLParameters via the
            // `(cipherSuites, protocols)` ctor populated from the engine's enabled
            // state (falling back to the rustls-negotiable defaults) so those
            // accessors return non-null arrays. ALPN still rides the objref-keyed
            // side-table below (getApplicationProtocols reads it regardless of how
            // the SSLParameters was constructed), so this does not regress ALPN.
            const DEFAULT_CIPHERS: &[&str] = SUPPORTED_CIPHER_SUITE_NAMES;
            let protocols = with_engine(id, |s| s.enabled_protocols.clone())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]);
            let ciphers = with_engine(id, |s| s.enabled_ciphers.clone())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| DEFAULT_CIPHERS.iter().map(|s| s.to_string()).collect());
            // `String[]` (i6-L2): the constructor's `setCipherSuites` /
            // `setProtocols` clone and `checkcast` these. `carr` is pinned
            // across the second build, which allocates.
            let carr = materialize_java_string_array(ctx, &ciphers);
            let carr_pin = ctx.pin_native_root(carr);
            let parr = Value::Object(Some(materialize_java_string_array(ctx, &protocols)));
            let carr = Value::Object(Some(ctx.read_native_pin(carr_pin, carr)));
            ctx.unpin_native_roots(carr_pin);
            // SSLParameters(String[] cipherSuites, String[] protocols)
            let p = match ctx.new_object_initialized(
                "javax/net/ssl/SSLParameters",
                "([Ljava/lang/String;[Ljava/lang/String;)V",
                &[carr, parr],
            )? {
                Some(Value::Object(Some(o))) => o,
                _ => try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4)?,
            };
            // Echo back the endpoint-identification algorithm this engine is
            // configured with. JSSE's contract is a round-trip
            // (`getSSLParameters` reports what `setSSLParameters` installed);
            // answering the constructor default here would tell an application
            // that reads its own configuration back that hostname verification
            // is off when it is on.
            let mut p = p;
            let names = with_engine(id, |s| match &s.explicit_sni {
                Some(names) => Some(names.clone()),
                None => s
                    .inferred_sni
                    .clone()
                    .unwrap_or_else(|| s.peer_host.clone().filter(|h| jsse_would_send_sni(h)))
                    .map(Some),
            })
            .flatten();
            if let Some(name) = names {
                let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
                let params_h = scope.root(p);
                let list = if let Some(name) = name {
                    // Computed text: a fresh `String`, as HotSpot's `SNIHostName` holds.
                    let host = scope.create_string_uninterned(&name);
                    let sni = scope.new_object_initialized(
                        "javax/net/ssl/SNIHostName",
                        "(Ljava/lang/String;)V",
                        &[Value::Object(Some(host))],
                    )?;
                    scope.invoke_static(
                        "java/util/Collections",
                        "singletonList",
                        "(Ljava/lang/Object;)Ljava/util/List;",
                        &[sni.unwrap_or(Value::Object(None))],
                    )?
                } else {
                    scope.invoke_static(
                        "java/util/Collections",
                        "emptyList",
                        "()Ljava/util/List;",
                        &[],
                    )?
                };
                p = scope.get(&params_h);
                if let Some(list) = list {
                    scope.invoke_virtual(p, "setServerNames", "(Ljava/util/List;)V", &[list])?;
                }
                p = scope.get(&params_h);
            }
            if let Some(alg) = with_engine(id, |s| s.endpoint_id_alg.clone()).flatten() {
                let p_pin = ctx.pin_native_root(p);
                let alg_str = ctx.create_string(&alg);
                p = ctx.read_native_pin(p_pin, p);
                let _ = ctx.invoke_virtual(
                    p,
                    "setEndpointIdentificationAlgorithm",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(alg_str))],
                );
                p = ctx.read_native_pin(p_pin, p);
                ctx.unpin_native_roots(p_pin);
            }
            // HotSpot's `SSLConfiguration` starts with
            // `SSLAlgorithmConstraints.DEFAULT` and `getSSLParameters()` hands it
            // back, so an engine's parameters never report `null` constraints
            // (SBR round 4, lane compat; probe `Sbr4CompatFamilies`
            // `engine.fresh.constraints`). Only where the JDK's own `Security`
            // runs (`JdkProviders`): the class's `<clinit>` reads the
            // `jdk.tls.disabledAlgorithms` security property, which the
            // `--compatible` stand-in `Security` never loaded. A custom
            // constraint object passed to `setSSLParameters` is not echoed back
            // (HELD page `SBR4-COMPAT-families-HELD-20261005.md`).
            if ctx.aligned(cratonvm_types::HotSpotAlignment::JdkProviders) {
                let p_pin = ctx.pin_native_root(p);
                let default_constraints = default_ssl_algorithm_constraints(ctx);
                p = ctx.read_native_pin(p_pin, p);
                if let Some(constraints) = default_constraints {
                    let _ = ctx.invoke_virtual(
                        p,
                        "setAlgorithmConstraints",
                        "(Ljava/security/AlgorithmConstraints;)V",
                        &[Value::Object(Some(constraints))],
                    );
                    p = ctx.read_native_pin(p_pin, p);
                }
                ctx.unpin_native_roots(p_pin);
            }
            // Stash ALPN onto the SSLParameters side-table so getApplicationProtocols echoes it.
            let alpn_list = with_engine(id, |s| {
                s.alpn_protocols
                    .iter()
                    .filter_map(|b| std::str::from_utf8(b).ok().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            // Key computed before the guard (gc-common w11-a). The row is
            // written even for an empty list: `setSSLParameters` on ANOTHER
            // engine applies a present-but-empty list (clearing that engine's
            // ALPN, as JSSE does) and leaves an absent one alone. See
            // `common-w11a-sslparameters-alpn-side-table-grows-and-serves-
            // reused-hashes` for the growth this cost; since gc-common w12-a
            // the row is swept once `p` dies (`gc_sweep_tls_rows`), which
            // bounds the table by the LIVE parameters objects and keeps a
            // later object with a reused hash from reading this list. `p` is
            // current here: it was re-read from its pin after the last
            // allocation, and `with_engine` above does not allocate.
            let p_key = engine_objref_key(ctx, p);
            sslparams_alpn_table().lock().insert(p_key, alpn_list);
            note_tls_weak_owner(ctx, TlsOwnerKind::SslParams, p_key, p);
            Ok(Some(Value::Object(Some(p))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `sun.security.ssl.SSLAlgorithmConstraints.DEFAULT`, the constraints every
/// JSSE engine starts with; `None` when the class cannot initialise here.
pub fn default_ssl_algorithm_constraints(ctx: &mut dyn NativeContext) -> Option<ObjectRef> {
    let cid = ctx
        .ensure_class_initialized("sun/security/ssl/SSLAlgorithmConstraints")
        .ok()?;
    let idx = ctx.static_field_index_by_name(cid, "DEFAULT")?;
    match ctx.get_static_field(cid, idx) {
        Value::Object(Some(o)) => Some(o),
        _ => None,
    }
}

pub fn sslparams_alpn_table() -> &'static parking_lot::Mutex<HashMap<u64, Vec<String>>> {
    static T: OnceLock<parking_lot::Mutex<HashMap<u64, Vec<String>>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

// =============================================================================
// Public entry points (called by the integrator from lib.rs)
// =============================================================================

/// WP5.1 — register all `sun.security.ssl.SSLEngineImpl` natives backed by a
/// real rustls connection. Must run AFTER `register_t27_natives` and AFTER
/// `register_p68_ssl` (in `phases_late.rs`) so the impl class shadows the
/// abstract `javax.net.ssl.SSLEngine` defaults. Callers outside the impl
/// class still hit the `phases_late.rs` 7-field stub paths.
/// Copy an `SSLContext`'s per-context identity onto the engine `createSSLEngine`
/// just produced, so `engine_begin` uses this engine's own keystore cert/key
/// (server cert, or client cert for mTLS) instead of the process-global slot.
pub fn set_engine_identity_override(
    ctx: &dyn NativeContext,
    engine_obj: ObjectRef,
    cert_pem: String,
    key_pem: String,
) {
    let id = engine_id_or_alloc(ctx, engine_obj);
    let trust_roots = take_selected_context_trust_roots();
    with_engine(id, |s| {
        s.identity_override = Some((cert_pem, key_pem));
        if let Some(trust_roots) = trust_roots {
            s.trust_roots_override = Some(trust_roots);
        }
    });
}

/// Record the peer host/port an application named in
/// `SSLContext.createSSLEngine(host, port)`.
///
/// Two things depend on it, and BOTH were silently wrong while this was
/// dropped on the floor (the `createSSLEngine(String,I)` native ignored its
/// arguments entirely):
///
///   1. **SNI.** `engine_begin` falls back to the literal `"localhost"` when no
///      peer host is known, so every client engine announced `localhost`
///      whatever host the caller actually dialled.
///   2. **Endpoint identification.** RFC 2818 hostname verification has nothing
///      to match against without the intended host — and "match against
///      `localhost`" is not a weaker check, it is a *wrong* one: it accepts a
///      `localhost` certificate for any host in the world. That is exactly the
///      shape of CVE-2018-8034, which Tomcat's `TestSecurity2018` regression-
///      tests by dialling `127.0.0.1` with a `localhost`-only certificate.
///
/// Also mirrored onto the Java object's own `peerHost`/`peerPort` fields where
/// they exist, so `SSLEngine.getPeerHost()`/`getPeerPort()` (plain JDK bytecode
/// reading final fields our synthetic allocation never ran a constructor for)
/// answer the same values rather than `null`/`-1`.
/// `SSLEngine.setUseClientMode(boolean)` — record the role AND the fact that
/// it was configured. See [`EngineState::client_mode_set`].
///
/// This replaces a `ctx.set_field(this, 0, ..)` in `phases_late::ssl_security`.
/// Slot 0 of a real `sun.security.ssl.SSLEngineImpl` is
/// `javax.net.ssl.SSLEngine.peerHost`, a REFERENCE — so the write was inert
/// and the read answered a String slot as a boolean. Same species as the
/// `SSLContext` slot-1 flag this lane found on 2026-09-12; the fix is the
/// same, which is to put the state where the engine's state already lives.
pub fn set_engine_use_client_mode(
    ctx: &dyn NativeContext,
    engine_obj: ObjectRef,
    use_client: bool,
) {
    let id = engine_id_or_alloc(ctx, engine_obj);
    with_engine(id, |s| {
        s.is_client = use_client;
        s.client_mode_set = true;
    });
}

/// `SSLEngine.getUseClientMode()` — the CONFIGURED role, `false` until
/// `setUseClientMode` has been called.
pub fn engine_use_client_mode(ctx: &dyn NativeContext, engine_obj: ObjectRef) -> bool {
    let id = engine_id_or_alloc(ctx, engine_obj);
    with_engine(id, |s| s.client_mode_set && s.is_client).unwrap_or(false)
}

pub fn set_engine_peer_host(
    ctx: &mut dyn NativeContext,
    engine_obj: ObjectRef,
    host: String,
    port: i32,
) {
    let id = engine_id_or_alloc(ctx, engine_obj);
    with_engine(id, |s| {
        s.peer_host = Some(host.clone());
        s.peer_port = port;
    });
    // `create_string` allocates, so it can relocate `engine_obj` under a moving
    // young collection — pin and re-read before the stores (family-1 shape: a
    // native local held live across an allocation).
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let engine_h = scope.root(engine_obj);
    let host_str = scope.create_string(&host);
    let host_h = scope.root(host_str);
    let inferred = match scope.invoke_static(
        "sun/security/ssl/Utilities",
        "rawToSNIHostName",
        "(Ljava/lang/String;)Ljavax/net/ssl/SNIHostName;",
        &[Value::Object(Some(host_str))],
    ) {
        Ok(Some(Value::Object(Some(name)))) => {
            match scope.invoke_virtual(name, "getAsciiName", "()Ljava/lang/String;", &[]) {
                Ok(Some(Value::Object(Some(value)))) => scope.read_string(value),
                _ => None,
            }
        }
        _ => None,
    };
    with_engine(id, |s| s.inferred_sni = Some(inferred));
    let engine_now = scope.get(&engine_h);
    let host_str = scope.get(&host_h);
    scope.set_field_by_name(engine_now, "peerHost", Value::Object(Some(host_str)));
    scope.set_field_by_name(engine_now, "peerPort", Value::Int(port));
}

/// Copy trust roots selected by the creating SSLContext even when it has no
/// identity. Pure client contexts otherwise fall back to platform roots when
/// their engine begins the handshake.
///
/// Also the engine's MINT POINT (gc-common w11-a): its one caller is the live
/// `SSLContext.createSSLEngine` (`net_phase_e::register_re6_ssl_context`),
/// which calls it first, on every path, on the object it has just allocated.
/// Any row still filed under that object's key belongs to a collected engine
/// that lived at the same address and whose key no sweep has freed yet (since
/// gc-common w26-b the key is the weak lock key, so a LIVE engine with the
/// same identity hash has a key of its own and is never touched here), so it
/// is dropped before the new engine's state is created -- see
/// [`engine_forget_key`]. A second caller added on an EXISTING engine would
/// wipe it: call `engine_id_or_alloc` + `with_engine` there instead.
pub fn set_engine_trust_roots_override(ctx: &dyn NativeContext, engine_obj: ObjectRef) {
    engine_forget_key(engine_objref_key(ctx, engine_obj));
    let id = engine_id_or_alloc(ctx, engine_obj);
    let trust_roots = take_selected_context_trust_roots();
    with_engine(id, |s| s.trust_roots_override = trust_roots);
}

/// Record which `SSLContext` (by its GC-stable key) created this engine, so
/// the post-handshake trust check can later look up that context's
/// `TrustManager[]` in `ctx_trust_managers_table`. Called alongside
/// `set_engine_identity_override` from `createSSLEngine`, but unconditionally
/// (a context can carry trust managers without carrying a KMF identity, e.g.
/// a pure client with no client certificate).
pub fn set_engine_trust_ctx_key(
    ctx: &mut dyn NativeContext,
    engine_obj: ObjectRef,
    ctx_obj: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let key = ctx_obj_key(ctx, ctx_obj)?;
    let engine_key = engine_objref_key(&*ctx, engine_obj);
    let id = engine_id_or_alloc(ctx, engine_obj);
    if crate::nbflags().dbg_tls_auth_ok {
        let has_entry = ctx_trust_managers_table().lock().contains_key(&key);
        eprintln!(
            "[dbg-tls-auth] set_engine_trust_ctx_key engine_id={} ctx_key={} table_has_entry={}",
            id, key, has_entry
        );
    }
    let recorded = with_engine(id, |s| {
        s.trust_managers_ctx_key = Some(key);
    });
    // gc-common w13-a: the engine now reads its context's rows, so they
    // outlive the context until this engine is gone
    // (`release_dead_ssl_contexts`). Only when the state really took the key.
    if recorded.is_some() {
        ssl_context_engine_hold(engine_key, key);
    }
    Ok(())
}

/// The `SNIMatcher`s a caller installed on a server engine via
/// `SSLParameters.setSNIMatchers` + `SSLEngine.setSSLParameters`, keyed by
/// `engine_objref_key`.
///
/// A matcher is arbitrary application code — `SNIMatcher.matches(SNIServerName)`
/// is abstract and netty's own tests subclass it inline — so the decision
/// cannot be precomputed in Rust from the `SSLParameters`; the objects have to
/// survive until the ClientHello arrives. That makes this the third
/// `ObjectRef`-holding table in this module, and it is scanned and remapped by
/// `gc_scan_tls_ctx_trust_manager_roots` / `gc_update_tls_ctx_trust_manager_refs`
/// below alongside the other two.
///
/// Before this existed, `setSSLParameters` read the ALPN list, the cipher
/// suites, the client-auth booleans and the endpoint-identification algorithm
/// off the `SSLParameters` and silently dropped everything else. A server
/// configured with a matcher that refuses every name still completed the
/// handshake — netty's `SniClientTest.testSniSNIMatcherDoesNotMatchClient`
/// asserts an `SSLException` and got `AssertionError: expected SSLException`.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0). Five sites: a `remove`, a
/// remove-or-insert whose guard covers only that choice, a
/// `match ..get(..).cloned()` that ends before the `ctx.create_string` below
/// it, and the GC scan/remap pair.
pub fn engine_sni_matchers_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<ObjectRef>>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<ObjectRef>>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// GC root scan for `ctx_trust_managers_table` — see the table's doc for why
/// this exists (the only ObjectRef-holding side-table in this module that
/// isn't purely derived PEM/DER bytes).
///
/// VM-scoped since gc-common w9-b: every table below is walked for VM `vm`'s
/// rows only. `SSLContext`-keyed rows name their VM through the lock-key
/// registry ([`ctx_key_owned_by`]), engine/session-keyed rows too (their key
/// is the object's weak lock key since gc-common w26-b;
/// [`objref_key_owned_by`]), `session_id_registry` values
/// and the two default slots record it directly.
pub fn gc_scan_tls_ctx_trust_manager_roots(vm: usize, roots: &mut Vec<ObjectRef>) {
    fn push(roots: &mut Vec<ObjectRef>, o: ObjectRef) {
        if !o.as_ptr().is_null() {
            roots.push(o);
        }
    }
    let table = ctx_trust_managers_table().lock();
    for (key, list) in table.iter() {
        if ctx_key_owned_by(*key, vm) {
            for tm in list {
                push(roots, *tm);
            }
        }
    }
    drop(table);
    scan_tls_engine_and_session_roots(vm, roots);
}

/// Every root of [`gc_scan_tls_ctx_trust_manager_roots`] except the
/// `SSLContext`-keyed `TrustManager` rows (gci i23/ep: split out unchanged, so
/// the ephemeron scan [`gc_scan_tls_ctx_roots_ephemeron`] shares it): the SNI
/// matchers, the rooted ALPN selectors, the engine sessions, the client
/// session cache, the session-id registry and the `HttpsURLConnection`
/// default factory.
pub fn scan_tls_engine_and_session_roots(vm: usize, roots: &mut Vec<ObjectRef>) {
    fn push(roots: &mut Vec<ObjectRef>, o: ObjectRef) {
        if !o.as_ptr().is_null() {
            roots.push(o);
        }
    }
    let matchers = engine_sni_matchers_table().lock();
    for (key, list) in matchers.iter() {
        if objref_key_owned_by(*key, vm) {
            for m in list {
                push(roots, *m);
            }
        }
    }
    drop(matchers);
    // gc-common w12-a: a consumed (or closed engine's) selector is not a root;
    // see `alpn_selector_release_root`. Read first, under its own lock: the
    // two `Scratch` locks are never held together.
    let unrooted: std::collections::HashSet<u64> = {
        let marks = alpn_selector_unrooted().lock();
        marks
            .iter()
            .copied()
            .filter(|k| objref_key_owned_by(*k, vm))
            .collect()
    };
    let selectors = engine_alpn_selector_table().lock();
    for (key, f) in selectors.iter() {
        if objref_key_owned_by(*key, vm) && !unrooted.contains(key) {
            push(roots, *f);
        }
    }
    drop(selectors);
    let sessions = engine_session_table().lock();
    for ((key, _), ses) in sessions.iter() {
        if objref_key_owned_by(*key, vm) {
            push(roots, *ses);
        }
    }
    drop(sessions);
    // The client session cache outlives the engine that created each entry —
    // that is its whole point — so it is an independent root, not something
    // `engine_session_table` keeps alive for it.
    let cached = client_session_cache().lock();
    for ((ctx_key, _, _), ses) in cached.iter() {
        if ctx_key_owned_by(*ctx_key, vm) {
            push(roots, *ses);
        }
    }
    drop(cached);
    // NOM-1 (G7-1). `session_id_registry` holds the same kind of live
    // `ObjectRef` as `engine_session_table`/`client_session_cache` above, and
    // is not implied by either — a resumed connection's session can outlive
    // the engine that negotiated it.
    let by_id = session_id_registry().lock();
    for ((owner, _), ses) in by_id.iter() {
        if *owner == vm {
            push(roots, *ses);
        }
    }
    drop(by_id);
    if let Some(f) = HUC_DEFAULT_FACTORY.peek(vm, |slot| *slot).flatten() {
        push(roots, f);
    }
}

/// The `SSLSocketFactory` last installed via
/// `HttpsURLConnection.setDefaultSSLSocketFactory`.
///
/// FIX (tls-handshake-enforcement-gap, doc 21): this used to be published
/// into the real JDK static field `HttpsURLConnection.defaultSSLSocketFactory`
/// so `http_url_connection::huc_client_tls_restrictions` could read it back.
/// Measured (`CRATONVM_DBG_TLS_AUTH`): the write never sticks — the read
/// immediately after reports `default factory is null` — so the probe that
/// applies a caller's client-side cipher/protocol restriction never ran at
/// all. Keep the reference here instead, where we control its lifetime.
///
/// Holds a live `ObjectRef`, so it MUST stay in the GC root set: it is
/// scanned and remapped by `gc_scan_tls_ctx_trust_manager_roots` /
/// `gc_update_tls_ctx_trust_manager_refs` above, alongside
/// `ctx_trust_managers_table` (the only other `ObjectRef`-holding table in
/// this module).
///
/// ONE SLOT PER VM since gc-common w9-b: VM B's `HttpsURLConnection` used VM
/// A's factory, an object in A's heap.
pub static HUC_DEFAULT_FACTORY: cratonvm_native_api::vm_scoped::VmScoped<Option<ObjectRef>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// Install VM `vm`'s default `SSLSocketFactory`.
pub fn set_huc_default_ssl_socket_factory(vm: usize, factory: ObjectRef) {
    HUC_DEFAULT_FACTORY.with(vm, |slot| *slot = Some(factory));
}

/// VM `vm`'s default `SSLSocketFactory`, if one was installed. The factory is
/// rooted and remapped only by its own VM's collection, and dropped at its
/// teardown.
pub fn huc_default_ssl_socket_factory_in_vm(vm: usize) -> Option<ObjectRef> {
    HUC_DEFAULT_FACTORY.peek(vm, |slot| *slot).flatten()
}

/// Remap one side-table reference through `map`.
#[inline]
pub fn remap_tls_ref(o: &mut ObjectRef, map: &cratonvm_types::PointerMap) {
    let old = o.as_ptr() as usize;
    if let Some(&new) = map.get(&old) {
        debug_assert!(new != 0, "GC pointer map contains null address");
        // SAFETY: `new` is a live, 8-byte-aligned heap address produced by the
        // moving collector for the object previously at `old`.
        *o = unsafe { ObjectRef::from_raw(new as *mut u8) };
    }
}

/// Post-move remap companion to `gc_scan_tls_ctx_trust_manager_roots`: the
/// same tables, the same VM `vm`'s rows only (gc-common w9-b) -- another VM's
/// pointer map describes another heap.
pub fn gc_update_tls_ctx_trust_manager_refs(vm: usize, map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let mut table = ctx_trust_managers_table().lock();
    for (key, list) in table.iter_mut() {
        if ctx_key_owned_by(*key, vm) {
            list.iter_mut().for_each(|tm| remap_tls_ref(tm, map));
        }
    }
    drop(table);
    let mut matchers = engine_sni_matchers_table().lock();
    for (key, list) in matchers.iter_mut() {
        if objref_key_owned_by(*key, vm) {
            list.iter_mut().for_each(|m| remap_tls_ref(m, map));
        }
    }
    drop(matchers);
    let mut selectors = engine_alpn_selector_table().lock();
    for (key, f) in selectors.iter_mut() {
        if objref_key_owned_by(*key, vm) {
            remap_tls_ref(f, map);
        }
    }
    drop(selectors);
    let mut sessions = engine_session_table().lock();
    for ((key, _), ses) in sessions.iter_mut() {
        if objref_key_owned_by(*key, vm) {
            remap_tls_ref(ses, map);
        }
    }
    drop(sessions);
    let mut cached = client_session_cache().lock();
    for ((ctx_key, _, _), ses) in cached.iter_mut() {
        if ctx_key_owned_by(*ctx_key, vm) {
            remap_tls_ref(ses, map);
        }
    }
    drop(cached);
    // NOM-1 (G7-1). Remap companion to the scan added above.
    let mut by_id = session_id_registry().lock();
    for ((owner, _), ses) in by_id.iter_mut() {
        if *owner == vm {
            remap_tls_ref(ses, map);
        }
    }
    drop(by_id);
    // Same treatment for the installed default `SSLSocketFactory` — see
    // `HUC_DEFAULT_FACTORY`.
    if HUC_DEFAULT_FACTORY.has_row(vm) {
        HUC_DEFAULT_FACTORY.with(vm, |slot| {
            if let Some(f) = slot.as_mut() {
                remap_tls_ref(f, map);
            }
        });
    }
    // gc-common w12-a: the weak owners are not roots, but a survivor's
    // address must follow it, or the next sweep would read it as dead. This
    // runs after `gc_sweep_tls_rows` in the stop-the-world epilogue, so every
    // address left here belongs to an object this collection kept.
    let mut owners = tls_weak_owners().lock();
    for row in owners.values_mut() {
        if row.vm == vm {
            row.addrs.iter_mut().for_each(|a| remap_tls_ref(a, map));
        }
    }
}

/// GC root scan for `ctx_key_managers_table` — mirrors
/// `gc_scan_tls_ctx_trust_manager_roots` exactly (see that table's doc),
/// including its VM scoping.
pub fn gc_scan_tls_ctx_key_manager_roots(vm: usize, roots: &mut Vec<ObjectRef>) {
    let table = ctx_key_managers_table().lock();
    for (key, list) in table.iter() {
        if !ctx_key_owned_by(*key, vm) {
            continue;
        }
        for km in list {
            if !km.as_ptr().is_null() {
                roots.push(*km);
            }
        }
    }
}

/// Post-move remap companion to `gc_scan_tls_ctx_key_manager_roots`.
pub fn gc_update_tls_ctx_key_manager_refs(vm: usize, map: &cratonvm_types::PointerMap) {
    if map.is_empty() {
        return;
    }
    let mut table = ctx_key_managers_table().lock();
    for (key, list) in table.iter_mut() {
        if ctx_key_owned_by(*key, vm) {
            list.iter_mut().for_each(|km| remap_tls_ref(km, map));
        }
    }
}

/// gci i23/ep (`common-w14d-proposal-ephemeron-native-side-tables`, the
/// Generational stop-the-world stage; switch `CRATONVM_GEN_SIDE_ROW_EPHEMERON`,
/// default OFF, read by the caller, `vm/src/memory/native_roots.rs`
/// `scan_tls`): [`gc_scan_tls_ctx_trust_manager_roots`] +
/// [`gc_scan_tls_ctx_key_manager_roots`] with the `SSLContext`-keyed manager
/// rows as EPHEMERONS.
///
/// A row of `ctx_trust_managers_table` / `ctx_key_managers_table` stands in
/// for `SSLContextImpl.trustManager` / `keyManager`, so on HotSpot its
/// managers are reachable while the context is (or an engine / socket made
/// from it, which reaches the context). Here a manager `deferrable` accepts
/// becomes one pin `(owner, manager)` per owning `SSLContext` address
/// ([`TlsOwnerKind::Context`] row of [`tls_weak_owners`]) in `pins` -- the
/// caller publishes them in `metadata_pin`, whose markers follow a pin from a
/// MARKED owner only -- when, and only when, nothing but the context reads
/// the key's rows: no engine and no [`CtxKeyLease`] holds the key
/// ([`CtxKeyHolders::holder_count`]), it is not marked dead, and no parked
/// socket names it ([`ssl_context_key_held_elsewhere`]). Those are exactly
/// the conditions under which [`release_dead_ssl_contexts`] drops the rows in
/// the SAME sweep that finds the context dead, so no row outlives a manager
/// this collection frees. Every other manager goes to `row_values` (rooted by
/// the caller, as before). Every non-context root goes to `roots` unchanged.
///
/// No table lock is held while `deferrable` or the parked-socket question
/// runs; the tables are read one at a time.
pub fn gc_scan_tls_ctx_roots_ephemeron(
    vm: usize,
    roots: &mut Vec<ObjectRef>,
    row_values: &mut Vec<ObjectRef>,
    deferrable: &dyn Fn(usize) -> bool,
    pins: &mut Vec<(usize, usize)>,
) {
    let mut rows: Vec<(u64, ObjectRef)> = Vec::new();
    for table in [ctx_trust_managers_table(), ctx_key_managers_table()] {
        let table = table.lock();
        for (key, list) in table.iter() {
            if ctx_key_owned_by(*key, vm) {
                rows.extend(
                    list.iter()
                        .filter(|o| !o.as_ptr().is_null())
                        .map(|o| (*key, *o)),
                );
            }
        }
    }
    if !rows.is_empty() {
        let mut keys: Vec<u64> = rows.iter().map(|(k, _)| *k).collect();
        keys.sort_unstable();
        keys.dedup();
        // Keys some holder besides the context reads: rooted.
        let candidates: Vec<u64> = {
            let holders = ctx_key_holders().lock();
            keys.into_iter()
                .filter(|k| holders.holder_count(*k) == 0 && !holders.dead.contains_key(k))
                .collect()
        };
        let candidates: Vec<u64> = candidates
            .into_iter()
            .filter(|k| !ssl_context_key_held_elsewhere(*k))
            .collect();
        // addr-keyed: local to this one root scan (built and dropped inside the pause, before
        // anything moves); keyed by context key, its values are this VM's `SSLContext` addresses
        // as the pause reads them, handed straight to the pin rows (gci i23/ep).
        let owners: HashMap<u64, Vec<usize>> = {
            let owners = tls_weak_owners().lock();
            candidates
                .iter()
                .filter_map(|k| {
                    let row = owners.get(&(TlsOwnerKind::Context, *k))?;
                    if row.vm != vm || row.addrs.is_empty() {
                        return None;
                    }
                    Some((
                        *k,
                        row.addrs
                            .iter()
                            .map(|a| a.as_ptr() as usize)
                            .collect::<Vec<usize>>(),
                    ))                })
                .collect()
        };
        for (key, value) in rows {
            let addr = value.as_ptr() as usize;
            match owners.get(&key) {
                Some(addrs) if deferrable(addr) => {
                    pins.extend(addrs.iter().map(|&owner| (owner, addr)));
                }
                _ => row_values.push(value),
            }
        }
    }
    scan_tls_engine_and_session_roots(vm, roots);
}

/// The process-wide default `SSLContext`, as most recently installed by
/// `SSLContext.setDefault(ctx)`. Holds the SAME live `ObjectRef` that was
/// passed to `setDefault` -- not a copy -- so that `SSLContext.getDefault()`
/// (see `net_phase_e.rs::register_re6_ssl_context`) can return an object that
/// already carries whatever per-context identity/trust-manager state
/// `SSLContext.init()` attached to it via `ctx_trust_managers_table`/
/// `ctx_key_managers_table` above (those tables are keyed off this same
/// object's GC-stable identity hash, so returning the identical object is
/// enough -- no data needs to be copied/re-attached here).
///
/// A moving GC can relocate the referenced object, so this raw `ObjectRef`
/// MUST stay in the GC root set: see `gc_scan_default_ssl_context_root` /
/// `gc_update_default_ssl_context_ref` below (wired into
/// `vm/src/memory/roots.rs` and `vm/src/memory/gc.rs`, mirroring
/// `ctx_trust_managers_table`'s wiring exactly).
///
/// ONE SLOT PER VM since gc-common w9-b
/// (`common-w8a-forkjoin-and-tls-root-rows-stay-process-global`): it was one
/// slot per process, so VM B's `SSLContext.getDefault()` returned VM A's
/// object -- an address in A's heap, rooted by B's collector and dangling
/// after A's teardown.
pub static DEFAULT_SSL_CONTEXT: cratonvm_native_api::vm_scoped::VmScoped<Option<ObjectRef>> =
    cratonvm_native_api::vm_scoped::VmScoped::new();

/// `SSLContext.setDefault(SSLContext)` (native registration lives in
/// `net_phase_e.rs`, alongside `getDefault`) -- install `ctx_obj` as VM
/// `vm`'s default so a subsequent `SSLContext.getDefault()` in that VM returns
/// this SAME object. See `DEFAULT_SSL_CONTEXT`'s doc for why identity, not a
/// copy, is what makes this work.
pub fn set_runtime_default_ssl_context(vm: usize, ctx_obj: ObjectRef) {
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] set_runtime_default_ssl_context ptr={:p}",
            ctx_obj.as_ptr()
        );
    }
    DEFAULT_SSL_CONTEXT.with(vm, |slot| *slot = Some(ctx_obj));
}

/// The `SSLContext` VM `vm` most recently installed via `setDefault`, if any
/// -- consulted by `SSLContext.getDefault()` so it returns the
/// caller-configured object instead of always allocating a fresh,
/// unconfigured one.
pub fn get_runtime_default_ssl_context(vm: usize) -> Option<ObjectRef> {
    DEFAULT_SSL_CONTEXT.peek(vm, |slot| *slot).flatten()
}

// =============================================================================
// Weak owners: rows whose lifetime is one Java object's (gc-common w12-a)
// =============================================================================
//
// `common-w11a-tls-session-objects-are-rooted-for-the-life-of-the-vm`,
// `common-w10e-sslengine-state-is-never-evicted`,
// `common-w11a-sslparameters-alpn-side-table-grows-and-serves-reused-hashes`,
// `common-w11a-huc-real-results-hold-every-response-body-for-the-life-of-a-vm`.
//
// Most TLS side tables are keyed by an object's (VM, identity hash), and
// nothing told them the object died, so a live VM never removed a row: an
// `SSLEngine`'s whole rustls connection, its two `SSLSession`s (GC roots), its
// ALPN selector (a GC root, and through netty's lambda the whole channel), an
// `SSLParameters`' ALPN list per `getSSLParameters()` call, and every real
// `HttpURLConnection`'s response body. A later object whose identity hash
// matched a dead one also inherited its rows. (Since gc-common w26-b the
// engine / socket / session / `SSLParameters` tables are keyed by the
// object's weak lock key instead -- see `gc_stable_objref_key` -- and a freed
// key's rows also go through `forget_tls_object_keys`.)
//
// The fix is the pattern gc-common w11-d landed for `Locale` rows
// (`lib.rs::gc_sweep_locale_rows`): the object that owns a family of rows is
// recorded here WEAKLY, by address, the moment its first row is written; the
// address is remapped with the rest of the `tls` root row
// (`gc_update_tls_ctx_trust_manager_refs`); and after every collection
// [`gc_sweep_tls_rows`] drops the rows of each owner the collecting VM did not
// keep. The owner is never a root, so the rows no longer keep it alive.
//
// What stays rooted, and why, row by row:
//
// * `engine_session_table` -- while its ENGINE lives. The engine object holds
//   no reference to its sessions (they live only here), and `getSession()`
//   must answer the same object for as long as the engine can be asked. The
//   engine's death drops the rows ([`engine_forget_key`]), and the sessions
//   then die with whatever else held them.
// * `engine_sni_matchers_table` -- while its engine lives, for the same reason
//   (the objects are consulted at the ClientHello and nothing on the Java side
//   need keep them).
// * `engine_alpn_selector_table` -- only until the selector has been CONSUMED
//   (the ClientHello it answers has arrived) or the engine is closed; see
//   [`alpn_selector_release_root`]. A rooted selector that captures its
//   engine (netty's `JdkAlpnSslEngine` lambda does) would otherwise keep the
//   engine reachable and the sweep could never fire.
// * `session_id_registry` -- JSSE's session cache: strong, but now bounded to
//   JSSE's default `getSessionCacheSize()` per VM
//   ([`SESSION_ID_REGISTRY_PER_VM`]).
// * `client_session_cache` and the `SSLContext`-keyed tables -- while their
//   CONTEXT lives, or any engine / `HttpsURLConnection` capture / parked socket
//   still names its key (gc-common w13-a; see "`SSLContext` rows" below).
// * the default `SSLContext` / `SSLSocketFactory` slots -- unchanged: one per
//   VM, replaced rather than accumulated.

/// The family of side-table rows a [`tls_weak_owners`] row stands for: the ONE
/// Java object whose death ends those rows' purpose.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum TlsOwnerKind {
    /// An `SSLEngine`: its `engine_table` / `engine_registry` rows (the whole
    /// rustls connection and its buffers), its cached `SSLSession`s, its ALPN
    /// selector and its SNI matchers ([`engine_forget_key`]).
    Engine,
    /// A synthetic `SSLSession`: the seven byte-only session side tables
    /// ([`forget_session_key`]).
    Session,
    /// An `SSLParameters`: its ALPN list in `sslparams_alpn_table`.
    SslParams,
    /// An `SSLContext` (gc-common w13-a): every row filed under its lock key
    /// ([`ctx_obj_key`]) -- its `TrustManager` / `KeyManager` objects (GC
    /// roots), identity, trust roots, session stores, cached client configs,
    /// the deferred-client-auth marker and its `client_session_cache` rows
    /// ([`forget_ssl_context_rows`]). Unlike the other kinds the owner's death
    /// alone does not end them: engines carry only the context's KEY and
    /// outlive it, so the rows go once the context is dead AND no engine,
    /// `HttpsURLConnection` capture or parked socket still names the key
    /// ([`release_dead_ssl_contexts`]).
    Context,
    /// A `KeyManagerState` (`x509_manager::km_registry`), keyed by its id
    /// (gc-common w13-a). Unlike every kind above the id has MANY carriers,
    /// each recorded here as one address: every object `set_km_id` stamps
    /// (the factory, each `KeyManager` it hands out) and every `PrivateKey`
    /// proxy `getPrivateKey` packs the id into. The state is offered for
    /// release once the last carrier dies ([`release_manager_state_candidates`]),
    /// and is kept while a factory row still names it.
    KeyManagerState,
    /// A `TrustManagerState` (`x509_manager::tm_registry`), keyed by its id;
    /// carriers are the objects `set_tm_id` stamps. Same rules as
    /// [`TlsOwnerKind::KeyManagerState`].
    TrustManagerState,
    /// An `SSLContext` or `SSLSocketFactory`'s custom trust anchors in
    /// `phases_late::ssl_security::p68_ctx_trust_roots_table` (gc-common
    /// w13-a; the table was keyed by raw address and never pruned). Since
    /// gc-common w27-a the key is the object's lock key, so the row names one
    /// object; it is the route that drops an `SSLContext`'s row, whose lock
    /// key this module HOLDS (`ctx_obj_key`), so the lock-key sweep never
    /// frees it.
    P68TrustRoots,
    /// An `HttpsURLConnection` given an instance `SSLSocketFactory`: its row
    /// in [`HUC_CONNECTION_CAPTURES`], keyed by the connection's weak lock key
    /// (gc-common w26-b; the row used to outlive its connection under the
    /// bare identity hash).
    HucConnection,
}

/// The live object(s) behind one `(kind, key)`. One address for the object
/// kinds (`Engine`, `Session`, `SslParams`, `HucConnection`, keyed by the
/// object's weak lock key since gc-common w26-b; `Context` and
/// `P68TrustRoots`, by its lock key), which no other live object shares; the
/// manager-state kinds list every carrier of the id, and the rows go only when
/// none survives. No kind is keyed by identity hash since gc-common w27
/// (w27-a / w27-b retired the factory and fallback-row kinds).
pub struct TlsWeakOwner {
    pub vm: usize,
    pub addrs: Vec<ObjectRef>,
}

/// `(kind, key)` -> the object(s) owning the rows filed under that key, where
/// `key` is the key the rows themselves use (the object's weak lock key,
/// [`gc_stable_objref_key`], or a manager-state id). NOT a root: remapped by
/// [`gc_update_tls_ctx_trust_manager_refs`] and swept by
/// [`gc_sweep_tls_rows`]. `LockLevel::Scratch`; no site holds it across a VM
/// re-entry or while taking another lock, and the sweep asks the heap only
/// after releasing it.
#[allow(clippy::type_complexity)]
pub fn tls_weak_owners(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<(TlsOwnerKind, u64), TlsWeakOwner>>
{
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<(TlsOwnerKind, u64), TlsWeakOwner>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record `owner` (in the calling VM's heap) as the object whose death ends
/// the `kind` rows filed under `key`. Idempotent per address.
///
/// `owner` must be the object's CURRENT address -- a native's argument, or a
/// reference re-read from a pin, before anything that can allocate. The same
/// holds for the identity-hash key computed from it at the same point, so a
/// caller that can compute the key correctly can also call this. A stale
/// address would read as dead at the next collection and drop live rows.
pub fn note_tls_weak_owner(
    ctx: &dyn NativeContext,
    kind: TlsOwnerKind,
    key: u64,
    owner: ObjectRef,
) {
    if owner.as_ptr().is_null() {
        return;
    }
    let vm = ctx.vm_identity();
    let mut owners = tls_weak_owners().lock();
    let row = owners.entry((kind, key)).or_insert_with(|| TlsWeakOwner {
        vm,
        addrs: Vec::with_capacity(1),
    });
    if !row.addrs.contains(&owner) {
        row.addrs.push(owner);
    }
}

/// Keys of engines whose ALPN selector is no longer a GC root: consumed by
/// the handshake, or the engine was closed before one arrived. The row itself
/// stays, so `getHandshakeApplicationProtocolSelector()` still answers the
/// selector for as long as something else keeps it alive (netty's engine
/// wrapper does), and [`gc_sweep_tls_rows`] drops the row once the selector
/// dies. `LockLevel::Scratch`, never nested.
pub fn alpn_selector_unrooted(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<u64>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<std::collections::HashSet<u64>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashSet::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Stop rooting engine `key`'s ALPN selector (see [`alpn_selector_unrooted`]).
///
/// Called once the selector can no longer be consulted: after
/// `engine_apply_alpn_selector` ran it against the ClientHello (it is run
/// once; the realised connection never asks again), and on `closeInbound` /
/// `closeOutbound` (a closed engine never handshakes). netty's server
/// selector is a lambda capturing its `JdkAlpnSslEngine` wrapper, which holds
/// the engine: rooted for good, it kept every such engine -- and its channel
/// -- reachable, so the engine's own rows could never be swept.
pub fn alpn_selector_release_root(key: u64) {
    let present = engine_alpn_selector_table().lock().contains_key(&key);
    if present {
        alpn_selector_unrooted().lock().insert(key);
    }
}

/// Whether engine `key`'s selector is still a root.
pub fn alpn_selector_is_rooted(key: u64) -> bool {
    !alpn_selector_unrooted().lock().contains(&key)
}

/// Drop the rows `kind` files under `key` in VM `vm` (their owner died), and
/// return how many rows went.
pub fn forget_rows_of_dead_owner(vm: usize, kind: TlsOwnerKind, key: u64) -> usize {
    match kind {
        TlsOwnerKind::Engine => engine_forget_key(key),
        TlsOwnerKind::Session => forget_session_key(key),
        TlsOwnerKind::SslParams => {
            usize::from(sslparams_alpn_table().lock().remove(&key).is_some())
        }
        // Only marked here: the rows may still serve a live engine.
        // [`release_dead_ssl_contexts`] drops them once nothing names the key.
        TlsOwnerKind::Context => {
            ctx_key_holders().lock().dead.insert(key, vm);
            0
        }
        // The last carrier of a manager id died: offered, and released at
        // the end of this sweep unless a factory row still names it.
        TlsOwnerKind::KeyManagerState | TlsOwnerKind::TrustManagerState => {
            offer_manager_state(vm, kind, key as u32 as i32);
            0
        }
        TlsOwnerKind::P68TrustRoots => {
            crate::phases_late::ssl_security::forget_p68_trust_roots(key)
        }
        TlsOwnerKind::HucConnection => forget_huc_connection_capture(vm, key),
    }
}

/// Drop VM `vm`'s instance capture filed under connection key `key` (the
/// connection died); answer how many rows went. The capture is dropped after
/// the table's guard is released: its `CtxKeyLease` takes `ctx_key_holders`.
pub fn forget_huc_connection_capture(vm: usize, key: u64) -> usize {
    if !HUC_CONNECTION_CAPTURES.has_row(vm) {
        return 0;
    }
    let removed = HUC_CONNECTION_CAPTURES.with(vm, |table| table.rows.remove(&key));
    let dropped = usize::from(removed.is_some());
    drop(removed);
    dropped
}

/// Drop VM `vm`'s TLS rows whose owning object did not survive a collection,
/// and the rows of unrooted ALPN selectors that died; answer how many rows
/// were dropped. Other VMs' rows are never judged.
///
/// `is_live(addr)` answers for an address as the tables hold it -- the
/// PRE-remap address inside a stop-the-world epilogue (a moved object is
/// live; [`gc_update_tls_ctx_trust_manager_refs`] re-addresses it later), or a
/// pure "not in a span this reclamation freed" range test. The same contract,
/// and the same three call sites, as `lib.rs::gc_sweep_locale_rows`.
///
/// `is_live` is NEVER evaluated under one of this module's locks: they are
/// `LockLevel::Scratch`, and the stop-the-world predicate takes the old-gen
/// lock. The addresses are copied out, judged, and the dead ones removed
/// under a second acquisition that re-checks them, so an owner recorded in
/// between (a mutator, during an out-of-pause reclamation) is never dropped.
/// The rows of a dead owner are then dropped table by table, one lock at a
/// time, none nested.
pub fn gc_sweep_tls_rows(vm: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    let candidates: Vec<((TlsOwnerKind, u64), ObjectRef)> = {
        let owners = tls_weak_owners().lock();
        owners
            .iter()
            .filter(|(_, row)| row.vm == vm)
            .flat_map(|(k, row)| row.addrs.iter().map(move |a| (*k, *a)))
            .collect()
    };
    let dead_addrs: Vec<((TlsOwnerKind, u64), ObjectRef)> = candidates
        .into_iter()
        .filter(|(_, a)| !is_live(a.as_ptr() as usize))
        .collect();
    let mut dead_keys: Vec<(TlsOwnerKind, u64)> = Vec::new();
    if !dead_addrs.is_empty() {
        // Grouped per row (gc-common w13-a): a manager id can have many
        // carriers, and one `retain` per dead address was quadratic in them.
        // Plain addresses, not an address-keyed `ObjectRef` table: a local
        // set dropped before this function returns.
        let mut by_row: HashMap<(TlsOwnerKind, u64), std::collections::HashSet<usize>> =
            HashMap::new();
        for (k, dead) in dead_addrs {
            by_row.entry(k).or_default().insert(dead.as_ptr() as usize);
        }
        let mut owners = tls_weak_owners().lock();
        for (k, dead) in by_row {
            let emptied = match owners.get_mut(&k) {
                Some(row) => {
                    row.addrs.retain(|a| !dead.contains(&(a.as_ptr() as usize)));
                    row.addrs.is_empty()
                }
                None => false,
            };
            if emptied {
                owners.remove(&k);
                dead_keys.push(k);
            }
        }
    }
    let mut dropped = 0usize;
    for (kind, key) in dead_keys {
        dropped += forget_rows_of_dead_owner(vm, kind, key);
    }
    // After the loop: this collection's dead engines have released their
    // context keys, and its dead carriers and factories have offered their
    // manager ids, whatever order the owners came out of the map in.
    dropped
        + sweep_unrooted_alpn_selectors(vm, is_live)
        + release_dead_ssl_contexts(vm)
        + release_manager_state_candidates(vm)
}

/// gci i30/rp: append every weak owner address of `vm`'s TLS rows to `out`,
/// read-only -- the addresses [`gc_sweep_tls_rows`] will judge, offered to
/// G1's kept-young-region verdict first (`crate::gc_weak_row_owners`). The
/// owner table lock (`Scratch`) is taken and released here, nothing else.
pub fn gc_weak_row_owners(vm: usize, out: &mut Vec<usize>) {
    let owners = tls_weak_owners().lock();
    out.extend(
        owners
            .values()
            .filter(|row| row.vm == vm)
            .flat_map(|row| row.addrs.iter().map(|a| a.as_ptr() as usize)),
    );
}

/// gcg g2/e — [`gc_sweep_tls_rows`] for the PURE RANGE TEST of a reclamation's
/// freed spans (`spans`: absolute `(start, len)`, sorted by start,
/// non-overlapping — `addr_keyed::drop_address_keyed_rows_in`'s contract).
/// `common-w26c-proposal-sub-linear-freed-span-drops`, item 2.
///
/// The general form copies every weak-owner address of the VM into a `Vec`
/// per call and judges it outside the lock, because its stop-the-world
/// predicate takes the old-gen lock. A range test takes no lock, so here it
/// runs UNDER the owner table's lock in one in-place pass — no copy, no
/// second acquisition — which is what the Generational concurrent sweep pays
/// once per slice. The same rows are dropped: an owner is dead iff its
/// address lies in a span, and a row whose last owner died is forgotten with
/// its row family, exactly as in [`gc_sweep_tls_rows`].
pub fn gc_drop_tls_rows_in_spans(vm: usize, spans: &[(usize, usize)]) -> usize {
    if spans.is_empty() {
        return 0;
    }
    let in_spans = |a: usize| {
        let i = spans.partition_point(|&(start, _)| start <= a);
        i > 0 && a - spans[i - 1].0 < spans[i - 1].1
    };
    let mut dead_keys: Vec<(TlsOwnerKind, u64)> = Vec::new();
    {
        let mut owners = tls_weak_owners().lock();
        owners.retain(|k, row| {
            if row.vm != vm {
                return true;
            }
            let before = row.addrs.len();
            row.addrs.retain(|a| !in_spans(a.as_ptr() as usize));
            if row.addrs.len() < before && row.addrs.is_empty() {
                dead_keys.push(*k);
                false
            } else {
                true
            }
        });
    }
    let mut dropped = 0usize;
    for (kind, key) in dead_keys {
        dropped += forget_rows_of_dead_owner(vm, kind, key);
    }
    dropped
        + sweep_unrooted_alpn_selectors(vm, &|a| !in_spans(a))
        + release_dead_ssl_contexts(vm)
        + release_manager_state_candidates(vm)
}

/// The selector half of [`gc_sweep_tls_rows`]: drop VM `vm`'s unrooted ALPN
/// selectors that died (and stale marks with no selector row).
pub fn sweep_unrooted_alpn_selectors(vm: usize, is_live: &dyn Fn(usize) -> bool) -> usize {
    let keys: Vec<u64> = {
        let unrooted = alpn_selector_unrooted().lock();
        if unrooted.is_empty() {
            return 0;
        }
        unrooted
            .iter()
            .copied()
            .filter(|k| objref_key_owned_by(*k, vm))
            .collect()
    };
    if keys.is_empty() {
        return 0;
    }
    let selectors: Vec<(u64, Option<ObjectRef>)> = {
        let table = engine_alpn_selector_table().lock();
        keys.iter().map(|k| (*k, table.get(k).copied())).collect()
    };
    let mut stale_marks = Vec::new();
    let mut dead = Vec::new();
    for (k, f) in selectors {
        match f {
            Some(f) if is_live(f.as_ptr() as usize) => {}
            Some(f) => dead.push((k, f)),
            None => stale_marks.push(k),
        }
    }
    let mut dropped = 0usize;
    if !dead.is_empty() {
        let mut table = engine_alpn_selector_table().lock();
        for (k, f) in &dead {
            // Only the selector that was judged: a setter may have installed
            // a new one in between (and re-rooted it).
            if table.get(k) == Some(f) {
                table.remove(k);
                dropped += 1;
                stale_marks.push(*k);
            }
        }
    }
    if !stale_marks.is_empty() {
        let mut unrooted = alpn_selector_unrooted().lock();
        for k in stale_marks {
            unrooted.remove(&k);
        }
    }
    dropped
}

/// Drop VM `vm`'s weak-owner rows and unrooted-selector marks (VM teardown,
/// via [`forget_vm_tls_state`]). The rows they stand for are dropped by the
/// tables' own teardown. Idempotent.
pub fn forget_vm_tls_weak_owners(vm: usize) {
    tls_weak_owners().lock().retain(|_, row| row.vm != vm);
    alpn_selector_unrooted()
        .lock()
        .retain(|k| !objref_key_owned_by(*k, vm));
    let mut holders = ctx_key_holders().lock();
    let engines: Vec<u64> = holders
        .engine_ctx
        .keys()
        .copied()
        .filter(|k| objref_key_owned_by(*k, vm))
        .collect();
    for engine_key in engines {
        holders.release_engine(engine_key);
    }
    holders.dead.retain(|_, owner| *owner != vm);
    drop(holders);
    manager_state_candidates()
        .lock()
        .retain(|(_, _, owner)| *owner != vm);
}

// -----------------------------------------------------------------------------
// `SSLContext` rows: gone with the context AND its last key holder
// (gc-common w13-a, `common-w12a-sslcontext-keyed-tls-rows-outlive-their-context`)
// -----------------------------------------------------------------------------
//
// Every per-context table is keyed by the context's lock key (`ctx_obj_key`)
// and nothing removed a row while the VM lived, so a server that builds an
// `SSLContext` per tenant / reload / test kept, per context ever built, its
// `TrustManager` and `KeyManager` objects (GC roots), a copy of its root set,
// a private-key PEM, two rustls session stores, a `ClientConfig` per engine
// shape and one rooted `SSLSession` per peer in `client_session_cache`.
//
// The context is recorded as the weak owner of its rows (`TlsOwnerKind::
// Context`) by the three `SSLContext.init` attaches, whose receiver is always
// re-read from a handle. Its death is not enough on its own: `createSSLEngine`
// hands an engine only the context's KEY (`EngineState::trust_managers_ctx_key`,
// see its doc for why never the objects) and the synthetic engine holds no
// Java reference to its context, so an application that builds a context,
// makes engines and drops the context still needs the rows for those
// engines' trust check, client-certificate choice and resumption. So:
//
// * each engine that carries a key is counted ([`CtxKeyHolders`]): held in
//   `set_engine_trust_ctx_key`, released by [`engine_forget_key`] (the
//   engine's death, or a new engine minted on its dead hash);
// * each `HttpsURLConnection` capture holds a [`CtxKeyLease`] on its keys,
//   including the clone an in-flight request keeps after its slot changed;
// * a dead context is only MARKED ([`forget_rows_of_dead_owner`]);
// * after each sweep [`release_dead_ssl_contexts`] drops the rows of every
//   marked context that no engine or lease counts and no parked socket
//   handshake will read.
//
// Getting a holder wrong is not a leak but a fail-OPEN: an engine, capture or
// socket whose context has `TrustManager`s runs a pass-through rustls
// verifier, and `engine_run_trust_check` accepts a key with no managers row.
// Every reader of a context key must therefore be a counted holder, a lease,
// or reachable only while the context object itself is (a `SSLSocketFactory`
// keeps its context in field 0).
//
// A context the lock-key registry later hands the same key (a new context on
// the dead one's identity hash) re-notes itself at its own `init`, which
// cancels the mark. A context never `init`-ed through these attaches is never
// recorded, and keeps today's behaviour: its (cache-only) rows stay.

/// Who still names an `SSLContext` key besides the context itself, and which
/// dead contexts are waiting for their last holder. `LockLevel::Scratch`;
/// never held while another lock is taken or the heap is asked.
#[derive(Default)]
pub struct CtxKeyHolders {
    /// Engine key ([`engine_objref_key`]) -> the context key its
    /// `EngineState::trust_managers_ctx_key` carries.
    pub engine_ctx: HashMap<u64, u64>,
    /// Context key -> how many rows of `engine_ctx` name it.
    pub engines: HashMap<u64, usize>,
    /// Context key -> how many live [`CtxKeyLease`]s name it.
    pub leases: HashMap<u64, usize>,
    /// Context key -> the VM whose collection found the context dead.
    pub dead: HashMap<u64, usize>,
}

/// Decrement `counts[key]`, dropping the entry at zero.
pub fn ctx_key_count_down(counts: &mut HashMap<u64, usize>, key: u64) {
    let emptied = match counts.get_mut(&key) {
        Some(n) => {
            *n = n.saturating_sub(1);
            *n == 0
        }
        None => false,
    };
    if emptied {
        counts.remove(&key);
    }
}

impl CtxKeyHolders {
    /// Forget that engine `engine_key` names a context. Idempotent.
    pub fn release_engine(&mut self, engine_key: u64) {
        if let Some(ctx_key) = self.engine_ctx.remove(&engine_key) {
            ctx_key_count_down(&mut self.engines, ctx_key);
        }
    }

    /// Engines and leases that still read `ctx_key`'s rows.
    pub fn holder_count(&self, ctx_key: u64) -> usize {
        self.engines.get(&ctx_key).copied().unwrap_or(0)
            + self.leases.get(&ctx_key).copied().unwrap_or(0)
    }
}

/// A native value that reads `SSLContext` rows by key (an `HttpsURLConnection`
/// capture) keeps them alive while it exists: counted on creation and on
/// every clone, released on drop. Taking [`ctx_key_holders`] in `drop` is
/// safe wherever a capture can be dropped -- that lock is a leaf, and no site
/// holds it while a capture goes.
#[derive(Default)]
pub struct CtxKeyLease(pub Vec<u64>);

impl CtxKeyLease {
    pub fn new(keys: impl IntoIterator<Item = u64>) -> Self {
        let mut held: Vec<u64> = keys.into_iter().collect();
        held.sort_unstable();
        held.dedup();
        if !held.is_empty() {
            let mut holders = ctx_key_holders().lock();
            for k in &held {
                *holders.leases.entry(*k).or_insert(0) += 1;
            }
        }
        CtxKeyLease(held)
    }
}

impl Clone for CtxKeyLease {
    fn clone(&self) -> Self {
        CtxKeyLease::new(self.0.iter().copied())
    }
}

impl Drop for CtxKeyLease {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let mut holders = ctx_key_holders().lock();
        for k in &self.0 {
            ctx_key_count_down(&mut holders.leases, *k);
        }
    }
}

pub fn ctx_key_holders() -> &'static cratonvm_types::lock_order::OrderedPlMutex<CtxKeyHolders> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<CtxKeyHolders>> = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            CtxKeyHolders::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Record `ctx_obj` (its CURRENT address; see [`note_tls_weak_owner`]) as the
/// weak owner of the rows filed under its key, and cancel a pending release
/// of that key (a new context the lock-key registry handed a dead one's key).
pub fn note_ssl_context_owner(ctx: &dyn NativeContext, key: u64, ctx_obj: ObjectRef) {
    note_tls_weak_owner(ctx, TlsOwnerKind::Context, key, ctx_obj);
    ctx_key_holders().lock().dead.remove(&key);
}

/// Engine `engine_key` now carries context key `ctx_key`.
pub fn ssl_context_engine_hold(engine_key: u64, ctx_key: u64) {
    let mut holders = ctx_key_holders().lock();
    if holders.engine_ctx.get(&engine_key) == Some(&ctx_key) {
        return;
    }
    holders.release_engine(engine_key);
    holders.engine_ctx.insert(engine_key, ctx_key);
    *holders.engines.entry(ctx_key).or_insert(0) += 1;
}

/// Engine `engine_key`'s rows are going (from [`engine_forget_key`]).
pub fn ssl_context_engine_release(engine_key: u64) {
    ctx_key_holders().lock().release_engine(engine_key);
}

/// Whether something neither counted as an engine nor holding a
/// [`CtxKeyLease`] still reads context key `key`'s rows: a socket whose
/// handshake was parked with the key (`phases_late::ssl_security`), whose
/// pass-through verifier leaves the chain to those `TrustManager`s.
pub fn ssl_context_key_held_elsewhere(key: u64) -> bool {
    crate::phases_late::ssl_security::parked_sockets_name_ctx_key(key)
}

/// Drop the rows of VM `vm`'s dead contexts that nothing names any more, and
/// answer how many rows went. Called at the end of [`gc_sweep_tls_rows`], so
/// a context whose last engine died in the same collection goes in it. Locks
/// are taken one at a time; the final re-check under [`ctx_key_holders`]
/// keeps a key that was re-noted or picked up by an engine in between.
pub fn release_dead_ssl_contexts(vm: usize) -> usize {
    let candidates: Vec<u64> = {
        let holders = ctx_key_holders().lock();
        if holders.dead.is_empty() {
            return 0;
        }
        holders
            .dead
            .iter()
            .filter(|(k, owner)| **owner == vm && holders.holder_count(**k) == 0)
            .map(|(k, _)| *k)
            .collect()
    };
    if candidates.is_empty() {
        return 0;
    }
    let revived: Vec<u64> = {
        let owners = tls_weak_owners().lock();
        candidates
            .iter()
            .copied()
            .filter(|k| owners.contains_key(&(TlsOwnerKind::Context, *k)))
            .collect()
    };
    let mut released: Vec<u64> = candidates
        .into_iter()
        .filter(|k| !revived.contains(k) && !ssl_context_key_held_elsewhere(*k))
        .collect();
    {
        let mut holders = ctx_key_holders().lock();
        for k in &revived {
            holders.dead.remove(k);
        }
        released.retain(|k| holders.dead.get(k) == Some(&vm) && holders.holder_count(*k) == 0);
        for k in &released {
            holders.dead.remove(k);
        }
    }
    released.into_iter().map(forget_ssl_context_rows).sum()
}

/// Drop every row filed under `SSLContext` key `key`, table by table, one lock
/// at a time; answer how many rows went. Nothing here reaches Java: the
/// values are PEM / DER bytes, rustls stores and configs, and the `ObjectRef`s
/// of the two rooted manager tables and `client_session_cache`, which simply
/// stop being roots.
pub fn forget_ssl_context_rows(key: u64) -> usize {
    let mut dropped = 0usize;
    dropped += usize::from(ctx_identity_table().lock().remove(&key).is_some());
    dropped += usize::from(ctx_trust_roots_table().lock().remove(&key).is_some());
    dropped += usize::from(ctx_trust_managers_table().lock().remove(&key).is_some());
    dropped += usize::from(ctx_key_managers_table().lock().remove(&key).is_some());
    dropped += usize::from(ctx_accepted_issuers_table().lock().remove(&key).is_some());
    dropped += usize::from(ctx_jsse_identifies_table().lock().remove(&key).is_some());
    dropped += usize::from(
        ctx_client_session_store_table()
            .lock()
            .remove(&key)
            .is_some(),
    );
    {
        let mut stores = ctx_server_session_store_table().lock();
        for client_auth in [false, true] {
            dropped += usize::from(stores.remove(&(key, client_auth)).is_some());
        }
    }
    {
        let mut configs = ctx_client_config_table().lock();
        let before = configs.len();
        configs.retain(|(k, _), _| *k != key);
        dropped += before - configs.len();
    }
    dropped += usize::from(deferred_client_auth_contexts().lock().remove(&key));
    {
        let mut cached = client_session_cache().lock();
        let before = cached.len();
        cached.retain(|(k, _, _), _| *k != key);
        dropped += before - cached.len();
    }
    // gc-common w14-c handoff: the context is dead and its rows are gone, so
    // nothing names its lock key any more; drop the key's tombstone.
    crate::release_lock_key(key);
    dropped
}

// -----------------------------------------------------------------------------
// Key-manager / trust-manager states: gone with their LAST carrier
// (gc-common w13-a, `common-w10e-tls-manager-and-keystore-registries-grow-for-
// the-life-of-a-vm`)
// -----------------------------------------------------------------------------
//
// A `KeyManagerState` / `TrustManagerState` id is carried by several Java
// objects at once -- the factory, every manager it hands out, every
// `PrivateKey` proxy packing a km id -- plus the `phases_late::ssl_security`
// factory rows. Freeing the state when ONE carrier dies would break the
// others, and a trust state freed under a live `TrustManager` is worse than a
// leak: `do_check_trusted` falls back to the platform roots, which WIDENS an
// explicit truststore. So:
//
// * every carrier is recorded, weakly, as one address of the id's
//   `(KeyManagerState | TrustManagerState, id)` weak-owner row -- by
//   `x509_manager::set_km_id` / `set_tm_id` (which write the id through the
//   same reference, so it is current by their own contract) and by
//   `make_private_key_mirror` on the proxy it just allocated;
// * the row empties when the last carrier dies; the id is then OFFERED
//   ([`offer_manager_state`]), as it is when a factory row naming it is
//   dropped or overwritten by a re-`init`;
// * at the end of each sweep [`release_manager_state_candidates`] frees an
//   offered state unless a carrier was recorded since or a factory row still
//   names it (`ssl_security::factory_rows_name_manager_id`).
//
// A state no carrier was ever recorded for, and no factory row offered, is
// never freed: the pre-w13 behaviour, never a premature release.

/// Manager states offered for release: `(kind, id, vm)`. `LockLevel::Scratch`;
/// never held while another lock is taken.
#[allow(clippy::type_complexity)]
pub fn manager_state_candidates() -> &'static cratonvm_types::lock_order::OrderedPlMutex<
    std::collections::HashSet<(TlsOwnerKind, i32, usize)>,
> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<
            std::collections::HashSet<(TlsOwnerKind, i32, usize)>,
        >,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            std::collections::HashSet::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Offer VM `vm`'s manager state `id` (`kind` is `KeyManagerState` or
/// `TrustManagerState`) for release at the end of the next sweep. Safe from a
/// mutator: nothing is freed here.
pub fn offer_manager_state(vm: usize, kind: TlsOwnerKind, id: i32) {
    if id == 0 {
        return;
    }
    manager_state_candidates().lock().insert((kind, id, vm));
}

/// Record `carrier` (CURRENT address) as an object that carries manager id
/// `id`, so the state outlives it. Called by `x509_manager::set_km_id` /
/// `set_tm_id`.
pub fn note_manager_id_carrier(
    ctx: &dyn NativeContext,
    state: TlsOwnerKind,
    id: i32,
    carrier: ObjectRef,
) {
    if id != 0 {
        note_tls_weak_owner(ctx, state, id as u32 as u64, carrier);
    }
}

/// `carrier` was re-stamped: it no longer carries manager id `id`. Removes its
/// address from the id's row and, if it was the last, offers the id.
pub fn forget_manager_id_carrier(
    ctx: &dyn NativeContext,
    state: TlsOwnerKind,
    id: i32,
    carrier: ObjectRef,
) {
    let key = (state, id as u32 as u64);
    let emptied = {
        let mut owners = tls_weak_owners().lock();
        let emptied = match owners.get_mut(&key) {
            Some(row) => {
                row.addrs.retain(|a| *a != carrier);
                row.addrs.is_empty()
            }
            None => false,
        };
        if emptied {
            owners.remove(&key);
        }
        emptied
    };
    if emptied {
        offer_manager_state(ctx.vm_identity(), state, id);
    }
}

