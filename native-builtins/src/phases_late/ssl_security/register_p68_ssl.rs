#[allow(unused_imports)]
use super::*;

pub fn register_p68_ssl(r: &mut NativeMethodRegistry) {
    // NEW-13: SSLContext = 5-field synthetic — see field layout constants.
    let ctx_class = "javax/net/ssl/SSLContext";
    r.register(
        ctx_class,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        |ctx, args| {
            // Validate protocol string: must be one of the recognised TLS
            // identifiers, otherwise throw NoSuchAlgorithmException like the
            // reference JDK does.
            let proto_ref = match args.get(0) {
                Some(Value::Object(Some(s))) => *s,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "SSLContext.getInstance: protocol is null".into(),
                    }
                    .into());
                }
            };
            let proto_name = ctx.read_string(proto_ref).unwrap_or_default();
            // W3-7: same pair of defects as the sibling in
            // `net_phase_e::register_re6_ssl_context`. The refusal was an
            // `IllegalArgumentException` — a RuntimeException, so *unchecked*,
            // sailing past `catch (NoSuchAlgorithmException)` even more quietly
            // than the IOException did. And this list was case-SENSITIVE, so
            // `getInstance("tls")` was refused although JCA lookup folds case.
            if !crate::jca::provider_chain::ssl_context_protocol_supported_in(
                ctx.vm_identity(),
                &proto_name,
            ) {
                return Err(crate::jca::provider_chain::throw_no_such_algorithm_public(
                    ctx,
                    &format!("{proto_name} SSLContext not available"),
                ));
            }
            // gc-common w15-d: the protocol String (an argument) is rooted
            // across the context's allocation, which the store follows.
            let obj: ObjectRef = p68_alloc_with_slot(
                ctx,
                "javax/net/ssl/SSLContext",
                NEW13_SSL_CTX_FIELDS,
                NEW13_CTX_PROTOCOL,
                Value::Object(Some(proto_ref)),
            )?;
            ctx.set_field(obj, NEW13_CTX_INIT, Value::Int(0));
            crate::jca::ssl_context_spi::mark_context_uninitialized(ctx, obj);
            ctx.set_field(obj, NEW13_CTX_KM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_TM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_RANDOM, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "getDefault",
        "()Ljavax/net/ssl/SSLContext;",
        |ctx, _args| {
            // A "Default" context is pre-initialised: it uses the platform
            // trust store and an implementation-defined `SecureRandom`.
            // gc-common w15-d: the String first, then the context (every store
            // went through the context's pre-allocation address).
            let obj: ObjectRef = p68_alloc_with_string_slot(
                ctx,
                "javax/net/ssl/SSLContext",
                NEW13_SSL_CTX_FIELDS,
                NEW13_CTX_PROTOCOL,
                "TLSv1.3",
            )?;
            ctx.set_field(obj, NEW13_CTX_INIT, Value::Int(1));
            crate::jca::ssl_context_spi::mark_context_initialized(ctx, obj);
            ctx.set_field(obj, NEW13_CTX_KM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_TM, Value::Object(None));
            ctx.set_field(obj, NEW13_CTX_RANDOM, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // NEW-13.2: SSLContext.init — actually validate the arguments, build a
    // `native_tls::TlsConnector` to prove the current platform can produce a
    // real TLS connector with the requested configuration, and record the
    // initialised state + KM/TM/random refs on the SSLContext instance.
    r.register(
        ctx_class,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        |ctx, args| {
            // Third-party SPI: its own `engineInit`. See
            // `jca::ssl_context_spi` for the boundary, and for why this guard
            // is on all three `javax/net/ssl/SSLContext` registration sets
            // rather than only on whichever wins last-write-wins today.
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineInit",
                "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
            ) {
                return r;
            }
            let this = obj_arg(args, 0)?;
            let km_arg = args.get(1).copied().unwrap_or(Value::Object(None));
            let tm_arg = args.get(2).copied().unwrap_or(Value::Object(None));
            let sr_arg = args.get(3).copied().unwrap_or(Value::Object(None));

            // Validate that arrays, if non-null, contain only KeyManager /
            // TrustManager references (i.e. Object-array element type).
            // Arity checks mirror the reference JDK: empty arrays are
            // allowed and treated equivalently to null.
            if let Value::Object(Some(km_arr)) = km_arg {
                // `array_length` panics on non-arrays, so gate behind a
                // type check via the underlying registry — but our
                // NativeContext trait doesn't expose is_array(). As a
                // defensive shortcut, simply try to read the length; if the
                // object isn't an array this will raise an exception via
                // the interpreter rather than crash the VM.
                let _ = ctx.array_length(km_arr);
            }
            if let Value::Object(Some(tm_arr)) = tm_arg {
                let _ = ctx.array_length(tm_arr);
            }

            // Capture the live manager objects before any helper below can
            // allocate or re-enter Java.  The native-call funnel keeps the
            // arguments rooted, but the copied ObjectRefs in `km_arg`/
            // `tm_arg` are not rewritten after a moving collection.  Delaying
            // this capture could therefore publish an old array element into
            // the long-lived TLS side table; a later handshake would then
            // dispatch `checkServerTrusted` on whatever object reused that
            // address.  The tables themselves are GC-rooted/remapped once
            // populated, so install them at this first post-validation point.
            let kms_array = match km_arg {
                Value::Object(Some(array)) => Some(array),
                _ => None,
            };
            let tms_array = match tm_arg {
                Value::Object(Some(array)) => Some(array),
                _ => None,
            };
            // FIX (jdkclienthttprequestfactory-certificaterequired-alert):
            // "capture before any helper can allocate" is not enough, because
            // the FIRST helper allocates — `attach_trust_managers_to_ctx`
            // calls `getAcceptedIssuers()` on every manager. From that point
            // on `this`, `km_arg` and `tm_arg` all name vacated slots, so the
            // second attach files the KeyManagers under a recycled object's
            // key (measured: two different keys inside one `init`, and a
            // `KeyManager[]` reading back length 0) and the field writes below
            // plant stale references in a live object. Root all three and
            // re-read at every use. Same defect, same call, as the sibling
            // handler in `net_phase_e.rs` — see its comment for the numbers.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this);
            let kms_h = kms_array.map(|a| scope.root(a));
            let tms_h = tms_array.map(|a| scope.root(a));
            let sr_h = match sr_arg {
                Value::Object(Some(o)) => Some(scope.root(o)),
                _ => None,
            };
            let this_now = scope.get(&this_h);
            let tms_now = tms_h.as_ref().map(|h| scope.get(h));
            crate::t27_tls::attach_trust_managers_to_ctx(&mut *scope, this_now, tms_now)?;
            let this_now = scope.get(&this_h);
            let kms_now = kms_h.as_ref().map(|h| scope.get(h));
            crate::t27_tls::attach_key_managers_to_ctx(&mut *scope, this_now, kms_now)?;
            let ctx = &mut scope;

            // FIX (es-restclient-https): if the supplied TrustManager[] is
            // bound to an explicit KeyStore (a custom truststore, not the
            // default), capture its trust anchors keyed by THIS SSLContext's
            // identity so getSocketFactory()/createSocket can add them to
            // the native-tls connector. See `p68_extract_trust_manager_roots`.
            let tm_arg = match tms_h.as_ref().map(|h| ctx.get(h)) {
                Some(a) => Value::Object(Some(a)),
                None => Value::Object(None),
            };
            let extra_roots = p68_extract_trust_manager_roots(&mut **ctx, tm_arg);
            // `p68_extract_trust_manager_roots` re-enters Java, so the address
            // this table is keyed by has to be taken from the handle AFTER it.
            let this = ctx.get(&this_h);
            // gc-common w13-a: keyed by a GC-stable key, not the address the
            // next moving collection invalidates, and swept with `this`
            // (gc-common w27-a: its lock key, no longer its identity hash).
            let ctx_key = p68_trust_roots_key(&**ctx, this);
            if let Some(roots) = extra_roots.clone() {
                p68_ctx_trust_roots_table().lock().insert(ctx_key, roots);
                note_p68_trust_roots_owner(&**ctx, ctx_key, this);
            } else {
                p68_ctx_trust_roots_table().lock().remove(&ctx_key);
            }

            // Prove we can build a real native-tls connector with the
            // platform trust store (plus any custom anchors) under the
            // currently-requested protocol. A failure here surfaces
            // immediately to the caller as a KeyManagementException-shaped
            // IOException.
            let init_max_protocol = match ctx.get_field(this, NEW13_CTX_PROTOCOL) {
                Value::Object(Some(s)) => ctx
                    .read_string(s)
                    .as_deref()
                    .and_then(new13_ctx_max_protocol),
                _ => None,
            };
            if let Err(msg) = new13_build_connector(
                extra_roots.as_deref().unwrap_or(&[]),
                false,
                init_max_protocol,
                // Buildability check only — the JSSE default store is
                // resolved per connection, not here.
                None,
            ) {
                return Err(RuntimeError::IOException {
                    message: format!("SSLContext.init: {}", msg),
                }
                .into());
            }

            // Every reference written here comes from a handle, not from the
            // pre-allocation copies: `new13_build_connector` and the reads
            // above can have moved all of them, and storing a stale reference
            // into a live object's field plants a dangling root.
            let this = ctx.get(&this_h);
            let from_handle = |h: Option<&cratonvm_native_api::NativeHandle>,
                               ctx: &cratonvm_native_api::NativeHandleScope|
             -> Value {
                match h.map(|h| ctx.get(h)) {
                    Some(a) => Value::Object(Some(a)),
                    None => Value::Object(None),
                }
            };
            let km_arg = from_handle(kms_h.as_ref(), ctx);
            let tm_arg = from_handle(tms_h.as_ref(), ctx);
            let sr_arg = from_handle(sr_h.as_ref(), ctx);
            ctx.set_field(this, NEW13_CTX_KM, km_arg);
            ctx.set_field(this, NEW13_CTX_TM, tm_arg);
            ctx.set_field(this, NEW13_CTX_RANDOM, sr_arg);
            ctx.set_field(this, NEW13_CTX_INIT, Value::Int(1));
            crate::jca::ssl_context_spi::mark_context_initialized(&**ctx, this);

            // Keep the legacy p68 context coherent with the rustls-backed
            // transport bridge.  A real-JDK dispatch may reach this handler
            // through an inherited/cached SSLContext call; without these
            // transfers, a Java-supplied TrustManager is retained only in the
            // synthetic fields and HttpURLConnection silently falls back to
            // the platform verifier.
            let kms_array = kms_h.as_ref().map(|h| ctx.get(h));
            // nio20 wave 4 (L4): see `t27_tls::attach_pending_identity_to_ctx`.
            let km_supplied = kms_array.is_some_and(|a| ctx.array_length(a) > 0);
            let resolved_identity =
                crate::x509_manager::resolved_identity_pem_for_key_manager_array(
                    &mut **ctx,
                    kms_array,
                );
            // Trust-side counterpart (jdk-only/P4A-TOMCAT-20260812-RETIRED-20260921.md defect 4): see
            // the doc on `attach_pending_identity_to_ctx`/
            // `resolved_trust_roots_for_trust_manager_array`.
            let tms_array = tms_h.as_ref().map(|h| ctx.get(h));
            // nio20 wave 5 (L5): the trust-side mirror of `km_supplied`.
            let tm_supplied = tms_array.is_some_and(|a| ctx.array_length(a) > 0);
            let resolved_trust_roots =
                crate::x509_manager::resolved_trust_roots_for_trust_manager_array(
                    &mut **ctx,
                    tms_array,
                );
            let this = ctx.get(&this_h);
            crate::t27_tls::attach_pending_identity_to_ctx(
                &mut **ctx,
                this,
                resolved_identity,
                resolved_trust_roots,
                km_supplied,
                tm_supplied,
            )?;
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "getSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                args,
                "engineGetSocketFactory",
                "()Ljavax/net/ssl/SSLSocketFactory;",
            ) {
                return r;
            }
            let this0 = obj_arg(args, 0)?;
            crate::jca::ssl_context_spi::require_context_initialized(ctx, this0)?;
            // gc-common w13-a: the receiver is pinned across the allocation
            // below and re-read. It used to be read from `args` AFTER it, so
            // a moving collection there stored a stale context address into
            // the new factory's field 0.
            let this_pin = ctx.pin_native_root(this0);
            // Field 0 is the originating SSLContext, matching the factory
            // shape consumed by the HttpURLConnection TLS bridge.
            let obj = match crate::tls_carriers::alloc_tls_carrier(
                ctx,
                crate::tls_carriers::SSL_SOCKET_FACTORY_PUBLIC,
                crate::tls_carriers::SSL_SOCKET_FACTORY_IMPL,
                1,
            ) {
                Ok(obj) => obj,
                Err(e) => {
                    ctx.unpin_native_roots(this_pin);
                    return Err(e.into());
                }
            };
            let this = ctx.read_native_pin(this_pin, this0);
            ctx.unpin_native_roots(this_pin);
            // FIX (es-restclient-https): carry this SSLContext's custom trust
            // anchors (if any) forward onto the factory so createSocket can
            // find them — createSocket only has `this` = the factory, not
            // the originating SSLContext.
            ctx.set_field(obj, 0, Value::Object(Some(this)));
            // gc-common w27-a: the context's row is looked up (never minted
            // here), and the factory's key is minted before the insert's guard.
            let roots = match existing_ssl_obj_key(&*ctx, this) {
                Some(ctx_key) => p68_ctx_trust_roots_table().lock().get(&ctx_key).cloned(),
                None => None,
            };
            if let Some(roots) = roots {
                let factory_key = p68_trust_roots_key(&*ctx, obj);
                p68_ctx_trust_roots_table()
                    .lock()
                    .insert(factory_key, roots);
                note_p68_trust_roots_owner(&*ctx, factory_key, obj);            }
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "getServerSocketFactory",
        "()Ljavax/net/ssl/SSLServerSocketFactory;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineGetServerSocketFactory",
                "()Ljavax/net/ssl/SSLServerSocketFactory;",
            ) {
                return r;
            }
            crate::jca::ssl_context_spi::require_context_initialized(ctx, obj_arg(_args, 0)?)?;
            let obj = crate::tls_carriers::alloc_tls_carrier(
                ctx,
                crate::tls_carriers::SSL_SERVER_SOCKET_FACTORY_PUBLIC,
                crate::tls_carriers::SSL_SERVER_SOCKET_FACTORY_IMPL,
                0,
            )?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            // Slot 0 is this registration's own synthetic protocol string; on
            // a REAL `javax.net.ssl.SSLContext` it is `provider`. See the
            // twin in `net_phase_e::register_re6_ssl_context`.
            if let Some(v) = crate::jca::ssl_context_spi::real_context_protocol(ctx, args) {
                return Ok(Some(v));
            }
            let this = obj_arg(args, 0)?;
            Ok(Some(ctx.get_field(this, 0)))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "getProvider",
        "()Ljava/security/Provider;",
        |ctx, args| crate::jca::ssl_context_spi::ssl_context_provider(ctx, args), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "createSSLEngine",
        "()Ljavax/net/ssl/SSLEngine;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineCreateSSLEngine",
                "()Ljavax/net/ssl/SSLEngine;",
            ) {
                return r;
            }
            let this = obj_arg(_args, 0)?;
            crate::jca::ssl_context_spi::require_context_initialized(ctx, this)?;
            let obj = ssleng_alloc(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ctx_class,
        "createSSLEngine",
        "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        |ctx, _args| {
            if let Some(r) = crate::jca::ssl_context_spi::spi_delegate(
                ctx,
                _args,
                "engineCreateSSLEngine",
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
            ) {
                return r;
            }
            let this = obj_arg(_args, 0)?;
            crate::jca::ssl_context_spi::require_context_initialized(ctx, this)?;
            let obj = ssleng_alloc(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // SSLSocketFactory
    let ssf = "javax/net/ssl/SSLSocketFactory";
    r.register(
        ssf,
        "getDefault",
        "()Ljavax/net/SocketFactory;",
        |ctx, _args| {
            // FIX (springprofilearbitertests-ssf-getdefault-no-context):
            // real `SSLSocketFactory.getDefault()` delegates to
            // `SSLContext.getDefault().getSocketFactory()`, so the returned
            // factory carries the default SSLContext at field 0 — exactly
            // like `SSLContext.getSocketFactory()` above. This registration
            // used to allocate a bare 0-field object, so any caller that
            // later reaches the layered `createSocket(Socket,String,int,
            // boolean)` overload (e.g. Apache HttpClient5's
            // `SSLConnectionSocketFactory.createLayeredSocket`, which builds
            // its default factory via this exact static call) hit that
            // overload's `ctx.get_field(factory, 0)` on a factory with no
            // field 0 at all, throwing `IllegalStateException("SSLSocketFactory
            // has no owning SSLContext")` instead of connecting — a real-JDK
            // A/B confirmed CratonVM-only failure.
            //
            // REGRESSION 2026-08-04: the fix below was correct and still
            // present, but `net_phase_e::register_re6_ssl_context` carried a
            // SECOND registration of this same triple that ran later and won
            // by last-registration-wins, handing back a factory whose field 0
            // was `None`. That duplicate is deleted; this is the single owner
            // (asserted by `registry_contracts.rs`). The body moved into
            // `t27_tls::default_ssl_socket_factory_obj` so the
            // `HttpsURLConnection` factory getters — which also used to mint
            // bare carriers — share one implementation.
            let obj = crate::t27_tls::default_ssl_socket_factory_obj(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssf,
        "getDefaultCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // E42: this was a SEVEN-element list — a narrower "defaults" set
            // that the oracle says does not exist. Measured, HotSpot
            // 25.0.3+9-LTS (`scratchpad/e42/E42FactoryDefaults.java`):
            // `getDefaultCipherSuites()` and `getSupportedCipherSuites()` are
            // element-wise EQUAL (31 each), and a fresh socket's
            // `getEnabledCipherSuites()` equals both. There is one list. A
            // caller intersecting its configuration against this accessor —
            // which is what the accessor is for — was told this VM cannot do
            // ChaCha20 or any CBC suite. See `jsse_supported_suite_name_array`.
            Ok(Some(Value::Object(Some(jsse_supported_suite_name_array(
                ctx,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssf,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // E42: was a 13-entry inline copy of
            // `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES` that had drifted two
            // entries behind it (the `TLS_DHE_RSA_*` pair), so this VM's
            // factory disclaimed two suites its own sockets negotiate — while
            // `SSLSocket.getSupportedCipherSuites` in this same file read the
            // constant and listed them. Same list, one spelling.
            Ok(Some(Value::Object(Some(jsse_supported_suite_name_array(
                ctx,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // FIX (h2-testnetutils-cipherfactory-createsocket-cast): the true
    // zero-arg `createSocket()` — an unconnected socket, connected later via
    // `Socket.connect(SocketAddress, int)`. Without its own registration
    // here, this call fell through to the ancestor `javax/net/SocketFactory`
    // native (`phases_early.rs::register_phase52_server_socket_factory`),
    // which allocates a plain `java/net/Socket` — `(SSLSocket)
    // f.createSocket()` then threw `ClassCastException` for every caller
    // using this JSSE-standard connect-later pattern (H2's
    // `CipherFactory.createSocket`/`NetUtils.createLoopbackSocket`; see
    // `bug-h2-netutils-dsa-privatekey-tls-unsupported-FIXED.md`'s residuals).
    r.register(ssf, "createSocket", "()Ljava/net/Socket;", |ctx, args| {
        release_dead_socket_listener_roots(ctx);
        let extra_roots = p68_factory_trust_roots(ctx, args)?;
        let java_tm_key = p68_factory_java_tm_key(ctx, args)?;
        let max_protocol = p68_factory_max_protocol(ctx, args);
        // gcd d1/e: plus the handshake-listener slot (see `alloc_ssl_socket`).
        let sock = alloc_ssl_socket(ctx, NEW13_SSL_SOCK_FIELDS)?;
        sock_set_field(&*ctx, sock, NEW13_SOCK_TLSID, Value::Int(-1));
        sock_set_field(&*ctx, sock, NEW13_SOCK_CLOSED, Value::Int(0));
        stash_pending_ssl_socket_connect_ctx(ctx, sock, extra_roots, java_tm_key, max_protocol);
        Ok(Some(Value::Object(Some(sock))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // NEW-13.3: SSLSocketFactory.createSocket(String host, int port) → SSLSocket.
    // Backed by `servlet::s2_tls_connect`, which performs a real native-tls
    // handshake and registers the resulting TLS stream in the unified
    // `s2_registry` socket table so that SSLSocket shares id space and
    // close semantics with plain `java.net.Socket`.
    r.register(
        ssf,
        "createSocket",
        "(Ljava/lang/String;I)Ljava/net/Socket;",
        |ctx, args| {
            let host_ref = match args.get(1) {
                Some(Value::Object(Some(r))) => *r,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLSocketFactory.createSocket: host is null".into()),
                    }
                    .into());
                }
            };
            let host = ctx.read_string(host_ref).unwrap_or_default();
            if host.is_empty() {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "SSLSocketFactory.createSocket: host is empty".into(),
                }
                .into());
            }
            let port_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(443);
            if !(0..=65535).contains(&port_i) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {}", port_i),
                }
                .into());
            }
            let extra_roots = p68_factory_trust_roots(ctx, args)?;
            let java_tm_key = p68_factory_java_tm_key(ctx, args)?;
            let max_protocol = p68_factory_max_protocol(ctx, args);
            new13_do_create_socket(
                ctx,
                &host,
                port_i as u16,
                &extra_roots,
                java_tm_key,
                max_protocol,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `SSLSocketFactory` redeclares the `InetAddress` forms abstract even
    // though `SocketFactory` has a bridge registration.  A synthetic P68
    // factory therefore resolves these calls at the abstract declaration
    // instead of inheriting the ancestor native, yielding AbstractMethodError.
    r.register(
        ssf,
        "createSocket",
        "(Ljava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| p68_create_socket_inet_address(ctx, args, 1, 2), cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssf,
        "createSocket",
        "(Ljava/lang/String;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| {
            let host_ref = match args.get(1) {
                Some(Value::Object(Some(reference))) => *reference,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLSocketFactory.createSocket: host is null".into()),
                    }
                    .into());
                }
            };
            let host = ctx.read_string(host_ref).unwrap_or_default();
            if host.is_empty() {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "SSLSocketFactory.createSocket: host is empty".into(),
                }
                .into());
            }
            let port = args.get(2).and_then(|value| value.as_int()).unwrap_or(443);
            if !(0..=65535).contains(&port) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {port}"),
                }
                .into());
            }
            let extra_roots = p68_factory_trust_roots(ctx, args)?;
            let java_tm_key = p68_factory_java_tm_key(ctx, args)?;
            let max_protocol = p68_factory_max_protocol(ctx, args);
            new13_do_create_socket(
                ctx,
                &host,
                port as u16,
                &extra_roots,
                java_tm_key,
                max_protocol,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssf,
        "createSocket",
        "(Ljava/net/InetAddress;ILjava/net/InetAddress;I)Ljava/net/Socket;",
        |ctx, args| p68_create_socket_inet_address(ctx, args, 1, 2), cratonvm_native_api::NativeKind::Bridge,
    );
    // createSocket(Socket wrapped, String host, int port, boolean autoClose).
    // Real JDK contract: this reuses `wrapped`'s existing TCP connection (a
    // fresh outbound connection would break both the layered-server case
    // below AND client-side proxy-tunnel/CONNECT layering) and defaults to
    // CLIENT mode — the caller may still flip it to SERVER mode via
    // `setUseClientMode(false)` before the handshake actually starts, which
    // is exactly the MockWebServer HTTPS-listener pattern
    // (`spring-boot-cloudfoundry-rerun-20260717-FIXED.md`).
    // Since the role isn't known yet at this call, the handshake itself is
    // deferred — see `t27_tls::{stash_pending_layered_socket,
    // set_pending_layered_socket_client_mode, drive_pending_layered_handshake}`
    // and this file's `ensure_layered_handshake_started` — to `startHandshake()`
    // or the first I/O call, whichever comes first (matching real JSSE, where
    // both implicitly trigger the handshake).
    r.register(
        ssf,
        "createSocket",
        "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
        |ctx, args| {
            let factory = obj_arg(args, 0)?;
            let wrapped = match args.get(1) {
                Some(Value::Object(Some(socket))) => *socket,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some(
                            "SSLSocketFactory.createSocket: wrapped Socket is null".into(),
                        ),
                    }
                    .into())
                }
            };
            // `host` may legitimately be null (real JDK: the socket's own
            // peer address is used for the handshake in that case) — Spring's
            // and MockWebServer's callers always pass a non-null String here,
            // but a null placeholder host is still a valid caller value we
            // must not NPE on; the DNS/SNI-name concept is meaningless for
            // the SERVER-mode case (MockWebServer) anyway, since the
            // handshake there uses `wrapped`'s existing connection, not a
            // hostname lookup.
            let host = match args.get(2) {
                Some(Value::Object(Some(host_ref))) => {
                    ctx.read_string(*host_ref).unwrap_or_default()
                }
                _ => String::new(),
            };
            let port_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(443);
            if !(0..=65535).contains(&port_i) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {}", port_i),
                }
                .into());
            }
            // A factory with no reachable owning `SSLContext` used to be a
            // hard `IllegalStateException` here. That is not a JDK-faithful
            // failure mode — no real `SSLSocketFactory` exists without a
            // context; `getDefault()` and every `SSLContext.getSocketFactory()`
            // carry one — so an empty field 0 always means one of OUR
            // synthetic carriers lost it, and the honest answer is the
            // process default context, which is what the real
            // `SSLSocketFactory.getDefault()` would have supplied anyway.
            //
            // Twice now a single mis-wired carrier has converted into an
            // exception thrown before any network I/O, taking out a whole
            // test family (2026-07-23 the bare 0-field `getDefault()`;
            // 2026-08-04 the duplicate registration that clobbered its fix).
            // Falling back cannot weaken trust: caller-supplied anchors are
            // resolved below by factory identity (`p68_factory_trust_roots`),
            // not through this context, and the default context validates
            // against the platform trust store — strictly stricter than a
            // permissive caller-installed TrustManager, never laxer.
            // gc-common w15-d: the default-context fallback below allocates,
            // and so does the socket at the end; the factory (re-read by the
            // two trust helpers through `args`), the wrapped socket and the
            // context are rooted across them.
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let factory_h = scope.root(factory);
            let wrapped_h = scope.root(wrapped);
            let ssl_context_obj = match scope.get_field(factory, 0) {
                Value::Object(Some(context)) => context,
                _ => {
                    if crate::nbflags().dbg_tls_auth_ok {
                        eprintln!(
                            "[dbg-tls-auth] createSocket(layered): factory has no field-0 \
                             SSLContext, falling back to the process default"
                        );
                    }
                    crate::t27_tls::default_ssl_context_or_create(&mut *scope)?
                }
            };
            let ssl_context_h = scope.root(ssl_context_obj);
            // The two trust helpers read the factory out of `args[0]`.
            let mut fresh_args = args.to_vec();
            fresh_args[0] = Value::Object(Some(scope.get(&factory_h)));
            let args = &fresh_args[..];
            // Same trust-anchor/TrustManager resolution `new13_do_create_socket`
            // (the other `SSLSocketFactory.createSocket` overloads, and this
            // one's own prior CLIENT-mode behavior before SERVER-mode reuse
            // was added) already uses successfully — `args[0]` is this
            // factory. Confirmed necessary: `ssl_context`-keyed resolution
            // alone (`t27_tls::context_trust_root_ders`) came up empty for
            // this SSLBundle scenario, whose anchors instead live in the
            // legacy p68 factory-identity-keyed table populated by
            // `SSLContext.getSocketFactory()`'s own "carry trust anchors
            // forward onto the factory" fix.
            let extra_roots = p68_factory_trust_roots(&mut *scope, args)?;
            let java_tm_key = p68_factory_java_tm_key(&mut *scope, args)?;
            let wrapped_now = scope.get(&wrapped_h);
            let ssl_context_now = scope.get(&ssl_context_h);
            let wrapped_now = scope.get(&wrapped_h);
            let inherited_local_port = scope
                .invoke_virtual(wrapped_now, "getLocalPort", "()I", &[])?
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let wrapped_now = scope.get(&wrapped_h);
            let inherited_timeout = scope
                .invoke_virtual(wrapped_now, "getSoTimeout", "()I", &[])?
                .and_then(|v| v.as_int())
                .unwrap_or(0);
            let wrapped_now = scope.get(&wrapped_h);
            let inherited_nodelay = scope
                .invoke_virtual(wrapped_now, "getTcpNoDelay", "()Z", &[])?
                .and_then(|v| v.as_int())
                .unwrap_or(0)
                != 0;
            let wrapped_now = scope.get(&wrapped_h);
            let inherited_keepalive = scope
                .invoke_virtual(wrapped_now, "getKeepAlive", "()Z", &[])?
                .and_then(|v| v.as_int())
                .unwrap_or(0)
                != 0;
            let wrapped_now = scope.get(&wrapped_h);
            let ssl_context_now = scope.get(&ssl_context_h);
            let pending_id = crate::t27_tls::stash_pending_layered_socket(
                &mut *scope,
                wrapped_now,
                ssl_context_now,
                host.clone(),
                port_i as u16,
                extra_roots,
                java_tm_key,
            )
            .map_err(|message| RuntimeError::IOException { message })?;
            let ssl_context_now = scope.get(&ssl_context_h);
            // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE], second
            // path): the immediate-connect overloads honour a version-pinned
            // `SSLContext.getInstance(...)` via `p68_factory_max_protocol` ->
            // native-tls, but this deferred overload hands the handshake to
            // rustls instead, which never saw that ceiling — a socket from a
            // `TLSv1.2` context still negotiated TLS 1.3. Seed the pending
            // entry's enabled-protocol list, the same field
            // `setEnabledProtocols` writes and `protocol_versions_for`
            // consumes, so both paths agree.
            if let Value::Object(Some(proto_ref)) =
                scope.get_field(ssl_context_now, NEW13_CTX_PROTOCOL)
            {
                if let Some(name) = scope.read_string(proto_ref) {
                    if new13_ctx_max_protocol(&name).is_some() {
                        crate::t27_tls::set_pending_layered_socket_protocols(
                            pending_id,
                            vec![name],
                        );
                    }
                }
            }
            // The host String first, rooted: the socket allocated after it is
            // the last GC point, so the socket needs no root of its own.
            let host_str = scope.create_string(&host);
            let host_h = scope.root(host_str);
            // gcd d1/e: plus the handshake-listener slot (see `alloc_ssl_socket`).
            let socket: ObjectRef = alloc_ssl_socket(&mut *scope, 5)?;
            // gc-common w16-d handoff: the socket owns its pending layered
            // entry, so its death (or `close()`) releases the parked stream.
            // gc-common w27-a: by its weak lock key, marked so the key sweep
            // knows to ask `t27_tls` ([`ssl_layered_socket_owners`]).
            let socket_key = ssl_sock_key(&*scope, socket);
            ssl_layered_socket_owners().lock().insert(socket_key);
            crate::t27_tls::set_pending_layered_socket_owner(pending_id, socket_key.1);
            let host_obj = scope.get(&host_h);
            let pending_tls_id = crate::servlet::PENDING_LAYERED_SOCK_ID_BASE + pending_id;
            // The real JDK SSLSocket field layout is not this synthetic
            // adapter's layout, so these Int field writes may be rejected.
            // Keep the stream id in the side table used by the SSLSocket I/O
            // natives; otherwise getInputStream/getOutputStream resolve -1.
            crate::net_phase_e::sock_set_for_create_with_local_port(
                &*scope,
                socket,
                port_i,
                inherited_local_port,
                pending_tls_id,
            );
            crate::net_phase_e::sock_set(&*scope, socket, |s| {
                s.read_timeout_ms = inherited_timeout;
                s.tcp_no_delay = inherited_nodelay;
                s.keep_alive = inherited_keepalive;
            });
            crate::net_phase_e::re1_with_raw_stream(pending_tls_id, |tcp| {
                tcp.set_read_timeout(if inherited_timeout == 0 {
                    None
                } else {
                    Some(std::time::Duration::from_millis(inherited_timeout as u64))
                })
            })
            .transpose()
            .map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;

            sock_set_field(
                &*scope,
                socket,
                NEW13_SOCK_HOST,
                Value::Object(Some(host_obj)),
            );
            sock_set_field(&*scope, socket, NEW13_SOCK_PORT, Value::Int(port_i));
            sock_set_field(
                &*scope,
                socket,
                NEW13_SOCK_TLSID,
                Value::Int(pending_tls_id),
            );
            sock_set_field(&*scope, socket, NEW13_SOCK_CLOSED, Value::Int(0));
            // No SSLSession yet — the handshake hasn't run. `getSession()`
            // (like real JSSE) implicitly starts the handshake if needed; see
            // its registration below.
            Ok(Some(Value::Object(Some(socket))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // If `this` (a `javax/net/ssl/SSLSocket`) is still a pending layered
    // socket (`createSocket(Socket wrapped, ...)`'s deferred-handshake
    // design — see that registration's doc comment), drive its handshake now
    // in whichever role `setUseClientMode` last left it in (default client),
    // update the socket's fields/side-table to the now-real rustls stream
    // id, and build its SSLSession. Returns the resolved (non-pending) tls
    // id either way — a socket that was never pending, or already
    // handshaked, is returned unchanged.
    fn ensure_layered_handshake_started(
        ctx: &mut dyn NativeContext,
        socket: ObjectRef,
    ) -> Result<i32, MethodCallFailed> {
        let tls_id = new13_resolve_tls_id(ctx, socket);
        if (crate::servlet::PENDING_CONNECT_SOCK_ID_BASE..crate::servlet::RUSTLS_SOCK_ID_BASE)
            .contains(&tls_id)
        {
            if crate::net_phase_e::sock_get(ctx, socket)
                .enabled_protocols
                .as_ref()
                .is_some_and(|p| !p.iter().any(|p| p == "TLSv1.3" || p == "TLSv1.2"))
            {
                return Err(crate::phases_early::throw_jca_exc(ctx, "javax/net/ssl/SSLHandshakeException", "No appropriate protocol (protocol is disabled or cipher suites are inappropriate)"));
            }
        }

        // The connect-path deferral (`SSLSocket.connect` parked the endpoint
        // instead of handshaking -- see `new13_ssl_socket_connect`). Its
        // handshake is the ORIGINAL eager one, `new13_connect_and_handshake`,
        // run unchanged and simply later; only the moment moved.
        if tls_id >= crate::servlet::PENDING_CONNECT_SOCK_ID_BASE
            && tls_id < crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
        {
            let connect_id = tls_id - crate::servlet::PENDING_CONNECT_SOCK_ID_BASE;
            let Some(p) = take_pending_connect_socket(connect_id) else {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLHandshakeException",
                    "connect-path socket handshake state missing",
                ));
            };
            // Pin across the handshake for the reason the layered branch
            // below documents: it parks in native code for a long time and a
            // moving young collection there relocates `socket`.
            let socket_pin = ctx.pin_native_root(socket);
            // Hand the connection `connect()` established to the handshake —
            // NOT a second one. See `PendingConnectSocket::tcp`.
            let protocols = crate::net_phase_e::sock_get(ctx, socket)
                .enabled_protocols
                .unwrap_or_default();
            let handshake = new13_connect_and_handshake_restricted_on(
                ctx,
                &p.host,
                p.port,
                &p.extra_roots,
                p.java_tm_key,
                p.max_protocol,
                p.tcp,
                &protocols,
                p.client_identity.as_ref(),
            );
            let socket = ctx.read_native_pin(socket_pin, socket);
            ctx.unpin_native_roots(socket_pin);
            let real_tls_id = handshake?;
            let socket = new13_finish_socket(ctx, socket, &p.host, p.port, real_tls_id)?;
            // A listener registered between `connect()` and the first I/O has
            // NOT missed this handshake -- unlike the `createSocket(host,
            // port)` path, which handshakes before it hands the socket back.
            new13_fire_handshake_completed(ctx, socket)?;
            return Ok(real_tls_id);
        }
        if tls_id < crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
            || tls_id >= crate::servlet::RUSTLS_SOCK_ID_BASE
        {
            return Ok(tls_id);
        }
        let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
        // Pure native socket I/O below (the actual TLS handshake). Mark this
        // thread blocked so a concurrent VM safepoint doesn't wait for it
        // while the peer's event loop waits for this handshake's reply.
        // Pin `socket` across the park: the handshake is long, a moving young
        // collection during it relocates the object, and EVERY write below
        // (`sock_set_for_create`, `NEW13_SOCK_TLSID`, the `SSLSession`) would
        // then land on a stale reference. The socket keeps its PENDING tls id,
        // so the next resolution drives the handshake again and finds the
        // pending entry already consumed — `SSLHandshakeException: layered
        // socket handshake state missing`, seen intermittently in
        // `shouldUpdateSslWhenReloadingSslBundles`. Same hazard
        // `new13_do_create_socket`'s blocking-region comment describes.
        let socket_pin = ctx.pin_native_root(socket);
        // Interpreter round i3 wave 81, lane L6: the handshake's socket reads
        // and writes run in natives on HotSpot: RUNNABLE in native.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let stream_result = crate::t27_tls::drive_pending_layered_handshake(pending_id);
        ctx.end_blocking_region();
        let socket = ctx.read_native_pin(socket_pin, socket);
        ctx.unpin_native_roots(socket_pin);
        // Match `new13_do_create_socket`'s existing contract: a rustls
        // handshake failure (rejected/mismatched certificate, no common
        // cipher suite, etc.) must surface as `SSLHandshakeException`, not a
        // generic `IOException` — callers (this test cluster's
        // `connectWithSslBundle`/`connectWithSslBundleAndOptionsMismatch`
        // among them) specifically assert on the JSSE exception type for an
        // intentionally-rejected connection.
        let stream_id = stream_result.map_err(|message| {
            crate::phases_early::throw_jca_exc(ctx, "javax/net/ssl/SSLHandshakeException", &message)
        })?;
        let real_tls_id = crate::servlet::RUSTLS_SOCK_ID_BASE + stream_id;
        let port = sock_field(&*ctx, socket, NEW13_SOCK_PORT)
            .as_int()
            .unwrap_or(0);
        let local_port = crate::net_phase_e::re1_with_raw_stream(real_tls_id, |tcp| {
            tcp.local_addr().map(|a| i32::from(a.port())).unwrap_or(0)
        })
        .unwrap_or_else(|| crate::net_phase_e::sock_get(ctx, socket).local_port);
        crate::net_phase_e::sock_set_for_create_with_local_port(
            ctx,
            socket,
            port,
            local_port,
            real_tls_id,
        );
        sock_set_field(&*ctx, socket, NEW13_SOCK_TLSID, Value::Int(real_tls_id));
        // The handshake above SUCCEEDED, so `rustls_session_info` returning
        // `None` here is an internal inconsistency, not an ordinary state —
        // but the session object still has to answer something, and the only
        // honest something is JSSE's "nothing negotiated" pair. `"TLS"` /
        // `"UNKNOWN"` were neither: not JSSE vocabulary, not a real suite
        // name, and `"TLS"` reads as a legitimate protocol because it is the
        // `SSLContext.getInstance` algorithm name. See
        // `JSSE_NULL_CIPHER_SUITE`.
        let (protocol, cipher, _alpn, _sni) = crate::t27_tls::rustls_session_info(stream_id)
            .unwrap_or_else(|| {
                (
                    JSSE_NULL_PROTOCOL.into(),
                    JSSE_NULL_CIPHER_SUITE.into(),
                    None,
                    None,
                )
            });
        // E42: `NEW13_SSL_SESS_FIELDS`, not a bare `3`. This was the one minter
        // of this shape that spelled its own width, so it would have kept
        // minting the pre-E42 layout — and `t27_tls`'s slot rules are keyed on
        // the WIDTH, so a stale 3 here means a socket that really did handshake
        // gets a session with no attribute slot while its siblings have one.
        // The literal is why this site needed finding at all; the constant is
        // why it cannot drift again.
        // gc-common w15-d: the socket, the session and the protocol String
        // are each held across a later allocation (the session and the two
        // Strings are three GC points), so all three are rooted and re-read;
        // every write below used to go through its pre-allocation address.
        let (socket_after, session) = {
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let socket_h = scope.root(socket);
            let session_new =
                crate::t27_tls::alloc_session_carrier(&mut *scope, NEW13_SSL_SESS_FIELDS)?;
            let session_h = scope.root(session_new);
            let protocol_str = scope.create_string(&protocol);
            let protocol_h = scope.root(protocol_str);
            let cipher_str = scope.create_string(&cipher);
            let session_now = scope.get(&session_h);
            let protocol_now = scope.get(&protocol_h);
            crate::t27_tls::sess_set(
                &*scope,
                session_now,
                NEW13_SESS_PROTO,
                Value::Object(Some(protocol_now)),
            );
            crate::t27_tls::sess_set(
                &*scope,
                session_now,
                NEW13_SESS_CIPHER,
                Value::Object(Some(cipher_str)),
            );
            crate::t27_tls::sess_set(
                &*scope,
                session_now,
                NEW13_SESS_TLSID,
                Value::Int(real_tls_id),
            );
            crate::t27_tls::sess_set(&*scope, session_now, NEW13_SESS_ATTRS, Value::Object(None));
            (scope.get(&socket_h), session_now)
        };
        let socket: ObjectRef = socket_after;
        // CLIENT-mode only (a server stream never has "peer certificates" in
        // this sense for our purposes here) — without this, a caller like
        // Apache HttpComponents' `AbstractClientTlsStrategy.verifySession()`,
        // which unconditionally checks `SSLSession.getPeerCertificates()`
        // right after every successful TLS upgrade, sees an empty chain and
        // throws `SSLPeerUnverifiedException` even though the handshake
        // itself succeeded.
        if let Some(chain) = crate::t27_tls::rustls_client_peer_cert_chain_der(stream_id) {
            crate::t27_tls::record_client_peer_chain(ctx, session, chain);
        }
        sock_set_field(
            &*ctx,
            socket,
            NEW13_SOCK_SESSION,
            Value::Object(Some(session)),
        );
        new13_record_client_session_peer(ctx, socket, session);
        // A real handshake just completed on this socket — this is the one
        // place in the SSLSocket bridge where that is true after Java has had
        // any chance to register a listener (`createSocket(host, port)`
        // handshakes before it returns the socket, so a listener added
        // afterwards has, correctly, missed it). Stock JSSE signals exactly
        // here; see `new13_fire_handshake_completed`.
        new13_fire_handshake_completed(ctx, socket)?;
        Ok(real_tls_id)
    }

    /// Resolve this `SSLSocket`'s `SSLSession`, rebuilding it from the live
    /// TLS stream when the stored field reads back as null.
    ///
    /// FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): `getSession()`
    /// used to return `sock_field(&*ctx, this, NEW13_SOCK_SESSION)` raw, and on
    /// the `createSocket(String, int)` path that field reads back **null** —
    /// so a plain client socket answered `getSession() == null` where JSSE
    /// guarantees a non-null session. `new13_finish_socket` does write the
    /// field, but as its own FIX comment records, `alloc_concurrent_synthetic`
    /// sizes this object with the REAL `javax/net/ssl/SSLSocket` layout and
    /// the layout guard silently drops writes whose slot type does not match;
    /// the same hazard that already forced `NEW13_SOCK_TLSID` into
    /// `net_phase_e`'s side table applies to the session slot.
    ///
    /// So do what the tls-id resolution already does: treat the field as a
    /// fast path and fall back to the authoritative source. `new13_resolve_tls_id`
    /// finds the stream via the side table, and `new13_alloc_ssl_session`
    /// rebuilds an equivalent session from it. The rebuilt object is written
    /// back to the field on the chance the slot is usable on this layout, but
    /// nothing depends on that write landing.
    ///
    /// When there is genuinely no stream (an unconnected socket) this returns
    /// the INVALID session JSSE returns, not null.
    ///
    /// FIX (E12): the sentence that used to stand here said "Returns
    /// `Value::Object(None)` only when there is genuinely no stream (an
    /// unconnected socket) — the one case where JSSE itself would hand back an
    /// invalid session rather than a real one." **It named the correct
    /// behaviour and then did the opposite.** `SSLSocket.getSession()` is
    /// contracted never to return null; measured on HotSpot 25.0.3+9-LTS
    /// (`scratchpad/e12/E12TwoDoors.java`, DOOR 2), a socket from the zero-arg
    /// `SSLSocketFactory.createSocket()` — which is exactly the object this
    /// file's own `createSocket()V` registration mints with
    /// `NEW13_SOCK_TLSID = -1` — answers:
    ///
    /// ```text
    ///   getSession()      = sun.security.ssl.SSLSessionImpl   (NOT null)
    ///   getCipherSuite()  = SSL_NULL_WITH_NULL_NULL
    ///   getProtocol()     = NONE
    ///   isValid()         = false
    ///   getId().length    = 0
    /// ```
    ///
    /// so returning null turned HotSpot's answer into a
    /// `NullPointerException` inside the caller, at the extremely common
    /// `socket.getSession().getCipherSuite()`. A missing answer is normally
    /// better than a wrong one; here it was neither — it was a crash where the
    /// oracle has a defined, greppable answer.
    fn new13_resolve_socket_session(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<Value, MethodCallFailed> {
        let stored = sock_field(&*ctx, this, NEW13_SOCK_SESSION);
        if matches!(stored, Value::Object(Some(_))) {
            return Ok(stored);
        }
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id < 0 {
            // Deliberately NOT written back to `NEW13_SOCK_SESSION`: a socket
            // that is connected later must build a REAL session then, and
            // caching the null session would pin the sentinel forever. HotSpot
            // does not cache it either — DOOR 3 of the same transcript shows
            // two `getSession()` calls on one unconnected socket returning two
            // different `SSLSessionImpl` instances.
            let null_session = new13_alloc_null_ssl_session(ctx, -1)?;
            return Ok(Value::Object(Some(null_session)));
        }
        let pin = ctx.pin_native_root(this);
        let session = new13_alloc_ssl_session(ctx, tls_id)?;
        let this = ctx.read_native_pin(pin, this);
        ctx.unpin_native_roots(pin);
        sock_set_field(
            &*ctx,
            this,
            NEW13_SOCK_SESSION,
            Value::Object(Some(session)),
        );
        new13_record_client_session_peer(ctx, this, session);
        Ok(Value::Object(Some(session)))
    }

    /// Deliver a `HandshakeCompletedEvent` to every listener registered on
    /// `socket`, for a handshake that has genuinely just completed.
    ///
    /// Call this only where new key material was actually negotiated. It is
    /// deliberately NOT called from `startHandshake()` on an established
    /// connection — see the `addHandshakeCompletedListener` registration.
    ///
    /// Deviation from stock JSSE, recorded because it is observable: the real
    /// JDK dispatches this from a fresh thread named
    /// `HandshakeCompletedNotify-Thread` (`sun.security.ssl.TransportContext.
    /// finishHandshake`), whereas this delivers synchronously on the thread
    /// that completed the handshake. A listener therefore sees a different
    /// `Thread.currentThread().getName()`, and a listener that blocks will
    /// block the handshaking call rather than a throwaway notifier thread.
    /// Synchronous delivery is chosen over spawning a Java thread from a
    /// native because it needs no `Runnable` shim class and cannot leak a
    /// thread when a listener throws; the event contents are identical.
    fn new13_fire_handshake_completed(
        ctx: &mut dyn NativeContext,
        socket: ObjectRef,
    ) -> Result<(), MethodCallFailed> {
        // gcd d1/e: a socket with a listener slot delivers from it. The array
        // is a snapshot (add and remove replace it, never mutate it); a
        // listener an earlier one removed, or a socket an earlier one closed,
        // is skipped by re-reading the CURRENT slot before each call, as the
        // side-table arm below skips a handle no longer filed.
        if let Some(slot) = ssl_sock_listener_slot(&*ctx, socket) {
            let Some(listeners) = ssl_sock_slot_array(&*ctx, socket, slot) else {
                return Ok(());
            };
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let socket_h = scope.root(socket);
            let listeners_h = scope.root(listeners);
            let session = new13_resolve_socket_session(&mut *scope, socket)?;
            let socket_now = scope.get(&socket_h);
            let event = scope.new_object_initialized(
                "javax/net/ssl/HandshakeCompletedEvent",
                "(Ljavax/net/ssl/SSLSocket;Ljavax/net/ssl/SSLSession;)V",
                &[Value::Object(Some(socket_now)), session],
            );
            let event_h = match event {
                Ok(Some(Value::Object(Some(event)))) => scope.root(event),
                _ => return Ok(()),
            };
            let len = {
                let arr = scope.get(&listeners_h);
                scope.array_length(arr)
            };
            for i in 0..len {
                let arr = scope.get(&listeners_h);
                let Value::Object(Some(listener)) = scope.get_array_element(arr, i) else {
                    continue;
                };
                let socket_now = scope.get(&socket_h);
                if ssl_sock_slot_position(&*scope, socket_now, slot, listener).is_none() {
                    continue;
                }
                let event_now = scope.get(&event_h);
                // A throwing listener neither aborts the handshake nor skips
                // the rest (see the side-table arm below).
                let _ = scope.invoke_virtual(
                    listener,
                    "handshakeCompleted",
                    "(Ljavax/net/ssl/HandshakeCompletedEvent;)V",
                    &[Value::Object(Some(event_now))],
                );
            }
            return Ok(());
        }
        // gc-common w27-a: a pure lookup -- a socket never keyed has no
        // listeners. `socket` is current at both callers (re-read after the
        // handshake's park), so the key is the socket's own.
        let Some(socket_key) = existing_ssl_obj_key(ctx, socket) else {
            return Ok(());
        };
        // Copy the handles out and release the lock BEFORE re-entering Java:
        // `handshakeCompleted` is arbitrary application code that can call
        // back into `add`/`removeHandshakeCompletedListener` on this same
        // socket, which would deadlock on a held non-reentrant mutex.
        let handles: Vec<usize> = {
            let table = handshake_listeners().lock();
            match table.get(&socket_key) {
                Some(entry) => entry.iter().map(|(_, handle)| *handle).collect(),
                None => return Ok(()),
            }
        };
        if handles.is_empty() {
            return Ok(());
        }
        // gc-common w15-d: one scope instead of hand-paired pins. The old
        // pair leaked the socket pin whenever `new13_resolve_socket_session`
        // failed (its `?` returned past the unpin), leaving the socket a GC
        // root for the rest of the thread's native frame.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let socket_h = scope.root(socket);
        let session = new13_resolve_socket_session(&mut *scope, socket)?;
        // `new13_resolve_socket_session` may allocate the session; nothing
        // collects between it and the constructor, whose funnel roots its
        // own arguments.
        let socket_now = scope.get(&socket_h);
        // `HandshakeCompletedEvent` is a real JDK class with real bytecode;
        // running its constructor is what makes `getSource()`/`getSession()`/
        // `getCipherSuite()` work for free rather than needing a synthetic
        // stand-in with hand-maintained field indices.
        let event = scope.new_object_initialized(
            "javax/net/ssl/HandshakeCompletedEvent",
            "(Ljavax/net/ssl/SSLSocket;Ljavax/net/ssl/SSLSession;)V",
            &[Value::Object(Some(socket_now)), session],
        );
        let event_h = match event {
            Ok(Some(Value::Object(Some(event)))) => scope.root(event),
            _ => return Ok(()),
        };
        for handle in handles {
            // gc-common w16-d: a listener that ran before this one may have
            // removed this one, or closed the socket (which releases every
            // listener root). A global-root handle is the address of a freed
            // heap box, which the next `add_global_root` -- anywhere in the
            // VM -- can hand out again for an unrelated object, and then
            // `handshakeCompleted` was sent to THAT object. Only a handle
            // still filed for this socket is resolved; a removed listener was
            // already skipped whenever its handle had not been reused.
            let still_filed = handshake_listeners()
                .lock()
                .get(&socket_key)
                .is_some_and(|entry| entry.iter().any(|(_, h)| *h == handle));
            if !still_filed {
                continue;
            }
            let Some(listener) = scope.resolve_global_root(handle) else {
                continue;
            };
            let event_now = scope.get(&event_h);
            // A listener that throws must not abort the handshake or swallow
            // the remaining listeners — stock JSSE runs them on a detached
            // notifier thread, where a throw is likewise invisible to the
            // handshaking code.
            let _ = scope.invoke_virtual(
                listener,
                "handshakeCompleted",
                "(Ljavax/net/ssl/HandshakeCompletedEvent;)V",
                &[Value::Object(Some(event_now))],
            );
        }
        Ok(())
    }

    /// Release the global roots held for `socket`'s handshake listeners.
    /// Called from `close()` so a long-lived process that opens many TLS
    /// sockets does not accumulate permanently-reachable listener objects.
    fn new13_drop_handshake_listeners(
        ctx: &mut dyn NativeContext,
        socket: ObjectRef,
    ) -> Result<(), MethodCallFailed> {
        // gcd d1/e: a socket with a listener slot drops its array (nothing
        // was rooted for it).
        if let Some(slot) = ssl_sock_listener_slot(&*ctx, socket) {
            ssl_sock_slot_clear(ctx, socket, slot);
            return Ok(());
        }
        let Some(socket_key) = existing_ssl_obj_key(ctx, socket) else {
            return Ok(());
        };
        let removed = handshake_listeners().lock().remove(&socket_key);
        let handles = match removed {
            Some(entry) => entry,
            None => return Ok(()),
        };
        for (_, handle) in handles {
            ctx.remove_global_root(handle);        }
        Ok(())
    }

    // SSLSocket methods
    let ssl_sock = "javax/net/ssl/SSLSocket";
    r.register(
        ssl_sock,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this_arg = obj_arg(args, 0)?;
            // Real JSSE: getSession() implicitly starts the handshake if one
            // hasn't run yet.
            //
            // gc-common w15-d: the handshake parks in native code and then
            // runs the listeners (Java), so the socket is rooted across it;
            // the session was resolved -- and cached -- through the address
            // read before it.
            let this_pin = ctx.pin_native_root(this_arg);
            let started = ensure_layered_handshake_started(ctx, this_arg);
            let this = ctx.read_native_pin(this_pin, this_arg);
            ctx.unpin_native_roots(this_pin);
            started?;
            let session = new13_resolve_socket_session(ctx, this)?;
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getSession sock={:?} -> {:?}",
                    std::thread::current().id(),
                    this,
                    session
                );
            }
            Ok(Some(session))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ssl_sock, "startHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // "Socket is not connected" comes FIRST, because there is nothing to
        // hand shake with. `SSLSocketFactory.getDefault().createSocket()`
        // returns an unconnected socket and HotSpot's `SSLSocketImpl
        // .startHandshake` refuses it with a `SocketException`; this returned
        // normally, which tells a caller a handshake completed on a socket
        // that has no peer. MEASURED, `L6TlsParamSweep` row 86.
        if !new13_socket_ever_connected(ctx, this) {
            return Err(crate::net_phase_e::re1_socket_exception(
                ctx,
                "Socket is not connected",
            ));
        }
        // A `tls_id` at or above `RUSTLS_SOCK_ID_BASE` is a connection whose
        // handshake has already completed, which is the ONLY case where
        // `startHandshake()` means "renegotiate". `ensure_layered_handshake_started`
        // returns immediately for it, so the call succeeds, changes nothing and
        // says nothing — and an application only calls it there to force fresh
        // key material.
        //
        // rustls implements no TLS 1.2 renegotiation (its manual lists that as
        // its mitigation for CVE-2009-3555 and 3SHAKE), so declining is right;
        // firing `HandshakeCompletedEvent` anyway would tell the caller key
        // material had been derived when none had. What is NOT right is doing
        // it silently. Once per process, at `warn`: `debug!` is compiled out of
        // shipping builds and would not be a signal at all.
        //
        // `TestSsl.testClientInitiatedRenegotiation[JSSE]` stays red on this
        // VM and cannot be otherwise: `TesterSupport.isClientRenegotiationSupported`
        // keys on Tomcat's `sslImplementationName` property alone and never
        // asks the platform, so no truthful capability report can reach it.
        // See known-issues/tomcat/ssl-renegotiation-emulation-limits.md.
        let established = new13_resolve_tls_id(ctx, this) >= crate::servlet::RUSTLS_SOCK_ID_BASE;
        ensure_layered_handshake_started(ctx, this)?;
        new13_enforce_endpoint_identification(ctx, this)?;
        if established {
            static WARNED: std::sync::atomic::AtomicBool =
                std::sync::atomic::AtomicBool::new(false);
            if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                tracing::warn!(
                    target: "tls",
                    "SSLSocket.startHandshake() on an already-established connection did \
                     nothing: this VM's TLS backend implements no TLS 1.2 renegotiation, so \
                     no fresh key material was derived and no HandshakeCompletedEvent will \
                     fire. The call is not an error and the connection stays usable. \
                     Reported once per process."
                );            }        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // These two real JDK `SSLSocket` methods originally had no native
    // registration at all, so calling either threw `AbstractMethodError: ...
    // has no Code attribute` (the abstract class has no bytecode of its own).
    // That was first patched into an inert no-op pair, which stopped the
    // crash but left a worse contract: a listener could be registered and was
    // then *never* invoked, on any code path, for any handshake — including
    // the ordinary initial one that has nothing to do with renegotiation.
    //
    // FIX (TestSsl.testClientInitiatedRenegotiation[JSSE]): store the
    // listeners for real and dispatch a genuine `HandshakeCompletedEvent`
    // whenever a handshake actually completes on the socket (see
    // `new13_fire_handshake_completed`, called from
    // `ensure_layered_handshake_started`).
    //
    // What still does NOT fire is a `startHandshake()` on an
    // already-established connection — i.e. a client-initiated renegotiation.
    // That is deliberate and is a property of the TLS backend, not an
    // oversight: rustls does not implement TLS 1.2 renegotiation, and its own
    // manual (`vendor/rustls-cbc/src/manual/tlsvulns.rs`) lists that omission
    // as its mitigation for CVE-2009-3555 and 3SHAKE. Firing the event
    // anyway would tell the application that fresh key material had been
    // derived when none had. See
    // `testssl-client-initiated-renegotiation-FIXED.md`.
    r.register(
        ssl_sock,
        "addHandshakeCompletedListener",
        "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = match args.get(1) {
                Some(Value::Object(Some(listener))) => *listener,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "listener is null".to_string(),
                    }
                    .into())
                }
            };
            release_dead_socket_listener_roots(ctx);
            // gcd d1/e: a socket with a listener slot keeps its listeners in
            // it, traced as its own field -- no global root, so a listener
            // that captures its socket no longer keeps the socket alive.
            if let Some(slot) = ssl_sock_listener_slot(&*ctx, this) {
                ssl_sock_slot_add(ctx, this, slot, listener)?;
                return Ok(None);
            }
            // gc-common w27-a: both keys are weak lock keys, minted here while
            // `this` and `listener` are the native's own arguments (current).
            // The listener's was its identity hash, so a second listener that
            // drew the first's hash was dropped as a "duplicate" and a remove
            // of either un-rooted the other. The socket's row goes with the
            // socket's key (gc-common w16-d: a socket that dies without
            // `close()` takes its row, and its listeners stop being roots).
            let listener_key = ssl_obj_key(ctx, listener).1;
            let socket_key = ssl_sock_key(ctx, this);
            // A global root, not a bare ObjectRef: this reference outlives the
            // current native call and is consumed by a later one, so a moving
            // collection in between would otherwise leave a stale pointer —
            // the `add_global_root` doc comment describes exactly this case.
            let handle = ctx.add_global_root(listener);
            let mut table = handshake_listeners().lock();
            let entry = table.entry(socket_key).or_default();
            // Real JSSE stores listeners in a `HashSet`, so re-adding the same
            // listener is idempotent rather than double-registering it.
            if entry.iter().any(|(key, _)| *key == listener_key) {
                drop(table);
                ctx.remove_global_root(handle);
                return Ok(None);
            }
            entry.push((listener_key, handle));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "removeHandshakeCompletedListener",
        "(Ljavax/net/ssl/HandshakeCompletedListener;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let listener = match args.get(1) {
                Some(Value::Object(Some(listener))) => *listener,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "listener is null".to_string(),
                    }
                    .into())
                }
            };
            release_dead_socket_listener_roots(ctx);
            // gcd d1/e: the listener slot, when the socket has one.
            if let Some(slot) = ssl_sock_listener_slot(&*ctx, this) {
                return if ssl_sock_slot_remove(ctx, this, slot, listener)? {
                    Ok(None)
                } else {
                    // Real JSSE throws for a listener that was never added.
                    Err(RuntimeError::IllegalArgumentException {
                        message: "listener not registered".to_string(),
                    }
                    .into())
                };
            }
            // gc-common w27-a: pure lookups. A listener or socket never keyed
            // was never registered.
            let keys = existing_ssl_obj_key(ctx, listener).zip(existing_ssl_obj_key(ctx, this));
            let removed = match keys {
                None => None,
                Some(((_, listener_key), socket_key)) => {
                    let mut table = handshake_listeners().lock();
                    match table.get_mut(&socket_key) {
                        Some(entry) => {
                            match entry.iter().position(|(key, _)| *key == listener_key) {
                                Some(pos) => {
                                    let (_, handle) = entry.remove(pos);
                                    if entry.is_empty() {
                                        table.remove(&socket_key);
                                    }
                                    Some(handle)
                                }
                                None => None,
                            }
                        }
                        None => None,
                    }
                }
            };
            match removed {
                Some(handle) => {
                    ctx.remove_global_root(handle);
                    Ok(None)
                }
                // Real JSSE throws for a listener that was never added.
                None => Err(RuntimeError::IllegalArgumentException {
                    message: "listener not registered".to_string(),
                }
                .into()),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // connect(SocketAddress[, int timeout]) — the other half of the zero-arg
    // `createSocket()` pattern registered on `SSLSocketFactory` above. Real
    // `javax.net.ssl.SSLSocket` does not redeclare `connect` (it stays
    // inherited, concrete, from `java.net.Socket`), but that ancestor's own
    // native (`net_phase_e.rs`'s `re1_connect_socket`) only performs a plain
    // TCP connect — registering here, on the more specific `SSLSocket` class,
    // intercepts first and additionally drives the TLS client handshake.
    r.register(
        ssl_sock,
        "connect",
        "(Ljava/net/SocketAddress;)V",
        new13_ssl_socket_connect, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "connect",
        "(Ljava/net/SocketAddress;I)V",
        new13_ssl_socket_connect, cratonvm_native_api::NativeKind::Bridge,
    );
    // setUseClientMode/getUseClientMode, setNeedClientAuth/getNeedClientAuth,
    // setWantClientAuth/getWantClientAuth — unlike getApplicationProtocol
    // (concrete-but-throws) these six are genuinely `abstract` in the real
    // `javax.net.ssl.SSLSocket` base class (only a concrete provider
    // subclass, e.g. SunJSSE's SSLSocketImpl, implements them). Since this
    // synthetic object's class literally IS `javax/net/ssl/SSLSocket`, an
    // `invokevirtual` against the abstract declaration throws
    // `AbstractMethodError` — thrown from `MockWebServer$SocketHandler.
    // handle()` immediately after `createSocket(Socket,...)` returns
    // (`sslSocket.setUseClientMode(false)`, unconditional, BEFORE any
    // ALPN/read/write call), caught by its generic `catch (Exception e)` and
    // logged at SEVERE — invisible under CratonVM's apparently-inert
    // `java.util.logging`, so the connection is silently abandoned having
    // never read the request or written a response. This, not the ALPN
    // accessors below, is the actual root cause of the request/response
    // exchange never happening for a server socket obtained via
    // `SSLSocketFactory.createSocket(Socket,...)` (MockWebServer's HTTPS
    // listener contract) — `getApplicationProtocol`/`getSSLParameters` are
    // still worth having registered (OkHttp calls them too, just later) but
    // execution never reached them without this fix.
    r.register(ssl_sock, "setUseClientMode", "(Z)V", |ctx, args| {
        // A socket from `createSocket(Socket wrapped, ...)` defers its actual
        // handshake (see that registration's doc comment) precisely so this
        // call can still decide client-vs-server mode; record it on the
        // pending state if the handshake hasn't started yet. A socket from
        // any other creation path (already handshaked, or never went through
        // the deferred path) has no pending entry — matches this file's
        // existing convention of silently ignoring a mode change once the
        // role is already fixed.
        let this = obj_arg(args, 0)?;
        let use_client = args.get(1).and_then(|v| v.as_int()).unwrap_or(1) != 0;
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
            && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
        {
            let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
            crate::t27_tls::set_pending_layered_socket_client_mode(pending_id, use_client);
        }
        // Record the role regardless of whether a pending entry existed, so
        // `getUseClientMode()` reports what the caller actually asked for
        // rather than the hard-coded `false` it used to return.
        ssl_sock_auth_update(ctx, this, |s| s.0 = i32::from(use_client));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssl_sock_auth_get(ctx, this).0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // STUB-REMOVAL (wave 2): these four were constant no-ops / constant `false`.
    // See `ssl_sock_auth_state` for why the request is recorded here and where
    // the enforcing rebuild actually happens (`SSLServerSocket`). JSSE's
    // documented exclusivity — `setNeedClientAuth(true)` clears `want`, and
    // vice versa — is honoured, matching the `SSLEngine` sibling below.
    r.register(ssl_sock, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ssl_sock_auth_update(ctx, this, |s| {
            s.1 = i32::from(v != 0);
            if v != 0 {
                s.2 = 0;            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssl_sock_auth_get(ctx, this).1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ssl_sock_auth_update(ctx, this, |s| {
            s.2 = i32::from(v != 0);
            if v != 0 {
                s.1 = 0;            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(ssl_sock_auth_get(ctx, this).2)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getApplicationProtocol/getHandshakeApplicationProtocol — the real
    // `javax.net.ssl.SSLSocket` base class's own body for these (unlike most
    // of its methods, which are abstract) is CONCRETE and just throws
    // `UnsupportedOperationException`; only a provider's concrete subclass
    // (SunJSSE's SSLSocketImpl) overrides it. Since this synthetic object's
    // class literally IS `javax/net/ssl/SSLSocket`, without a native
    // registration here that base-class bytecode runs and throws. Callers
    // like OkHttp's `Platform.getSelectedProtocol()` (used by MockWebServer's
    // connection handler for ALPN bookkeeping right after `startHandshake()`)
    // catch that as a generic `Exception`, log it at FINE/SEVERE (invisible
    // by default under java.util.logging), and abandon the connection having
    // never read the request or written a response — surfaced to the client
    // as a silent hang (e.g. Reactor's `.block(Duration)` timing out).
    fn ssl_sock_negotiated_alpn(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<Option<String>, MethodCallFailed> {
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE {
            return Ok(None);
        }
        let raw_id = tls_id - crate::servlet::RUSTLS_SOCK_ID_BASE;
        Ok(crate::t27_tls::rustls_session_info(raw_id).and_then(|(_, _, alpn, _)| alpn))
    }
    r.register(
        ssl_sock,
        "getApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let alpn = ssl_sock_negotiated_alpn(ctx, this)?.unwrap_or_default();
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getApplicationProtocol sock={:?} -> {:?}",
                    std::thread::current().id(),
                    this,
                    alpn
                );
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&alpn)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getHandshakeApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let alpn = ssl_sock_negotiated_alpn(ctx, this)?.unwrap_or_default();
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getHandshakeApplicationProtocol sock={:?} -> {:?}",
                    std::thread::current().id(),
                    this,
                    alpn
                );
            }
            Ok(Some(Value::Object(Some(ctx.create_string(&alpn)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // getSSLParameters/setSSLParameters — like getApplicationProtocol above,
    // the real `javax.net.ssl.SSLSocket` base class's default bodies call
    // through to other (mostly abstract-in-the-base-class) accessors; without
    // a concrete provider subclass backing this synthetic object, that chain
    // is liable to throw. OkHttp's `Jdk9Platform.configureTlsExtensions()`
    // calls `getSSLParameters()` then `setSSLParameters()` on every socket
    // BEFORE `startHandshake()` (to offer its ALPN protocol list) — for
    // MockWebServer's synthetic server socket the handshake has already run
    // synchronously inside `createSocket`, so this call is moot for actual
    // negotiation, but it must not throw or the caller (uncaught) abandons
    // the connection having never read the request or written a response.
    r.register(
        ssl_sock,
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getSSLParameters ENTER sock={:?}",
                    std::thread::current().id(),
                    this
                );
            }
            // gc-common w15-d: everything keyed by the socket is read first,
            // while `this` is current. The rest allocates (the two lists, the
            // parameters object) and runs Java (its constructor and setters):
            // the socket used to be keyed after all of that, the cipher list
            // was held across the protocol list's allocations, and the
            // parameters object across a String allocation.
            let (_, need, want) = ssl_sock_auth_get(ctx, this);
            let alg = match existing_ssl_obj_key(ctx, this) {
                Some(alg_key) => ssl_sock_endpoint_alg().lock().get(&alg_key).cloned(),
                None => None,
            };
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            // E42 (E12-1 residual 5): this built a ONE-element array from the
            // SESSION's negotiated protocol, falling back to a hard-coded
            // `"TLSv1.3"`. Two defects in one expression, and both are the
            // category error E12 fixed in `getEnabledProtocols` next door:
            // `SSLParameters` describes CONFIGURATION, not what a handshake
            // produced, and the fallback announced this VM's preferred version
            // as though it were an outcome.
            //
            // Measured, HotSpot 25.0.3+9-LTS
            // (`scratchpad/e42/E42EnabledSets.java`), on an unconnected socket
            // and on a socket from a version-pinned context:
            //
            //     getSSLParameters().getProtocols() = [TLSv1.3, TLSv1.2]
            //     getEnabledProtocols()             = [TLSv1.3, TLSv1.2]
            //     equal (element-wise)              = true
            //     pinned to TLSv1.2: BOTH           = [TLSv1.2]
            //
            // The two accessors are the same answer through two doors, so they
            // are now one call. That also carries E12's sentinel filter here
            // for free: reading `NEW13_SESS_PROTO` raw would have reported
            // `["NONE"]` for an unconnected socket once
            // `new13_resolve_socket_session` started returning the null session
            // — the exact unmasking E12-1 §5 records for site 8, in the door it
            // did not check.
            let protocols_arr = ssl_sock_enabled_protocols(&mut *scope, this)?;
            let protocols_h = scope.root(protocols_arr);
            let ciphers = ssl_sock_supported_cipher_suites(&mut *scope)?;
            let protocols = scope.get(&protocols_h);
            let params_obj = match scope.new_object_initialized(
                "javax/net/ssl/SSLParameters",
                "([Ljava/lang/String;[Ljava/lang/String;)V",
                &[Value::Object(Some(ciphers)), Value::Object(Some(protocols))],
            )? {
                Some(Value::Object(Some(o))) => o,
                _ => try_alloc_concurrent_synthetic(&mut *scope, "javax/net/ssl/SSLParameters", 4)?,
            };
            // The rest of the CONFIGURATION this socket carries. The object
            // above described only the two lists, so a caller who had just
            // called `setSSLParameters` read back neither its client-auth
            // choice nor its endpoint-identification algorithm — see
            // `ssl_sock_endpoint_alg`.
            let params_h = scope.root(params_obj);
            if need != 0 {
                let params = scope.get(&params_h);
                let _ = scope.invoke_virtual(params, "setNeedClientAuth", "(Z)V", &[Value::Int(1)]);
            } else if want != 0 {
                let params = scope.get(&params_h);
                let _ = scope.invoke_virtual(params, "setWantClientAuth", "(Z)V", &[Value::Int(1)]);
            }
            if let Some(alg) = alg {
                let s = scope.create_string(&alg);
                let params = scope.get(&params_h);
                let _ = scope.invoke_virtual(
                    params,
                    "setEndpointIdentificationAlgorithm",
                    "(Ljava/lang/String;)V",
                    &[Value::Object(Some(s))],
                );
            }
            Ok(Some(Value::Object(Some(scope.get(&params_h)))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "setSSLParameters",
        "(Ljavax/net/ssl/SSLParameters;)V",
        |ctx, args| {
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} setSSLParameters sock={:?}",
                    std::thread::current().id(),
                    args.first()
                );
            }
            // A socket from `createSocket(Socket wrapped, ...)`'s deferred
            // handshake honors any cipher-suite restriction carried by
            // `params` if called before the handshake actually starts — the
            // same real-JSSE-contract reasoning as `setEnabledCipherSuites`
            // above (which this often replaces: Apache HttpComponents 5
            // configures TLS options via an `SSLParameters` object and
            // `SSLSocket.setSSLParameters()`, not the legacy per-field
            // setters, so `setEnabledCipherSuites` alone never saw
            // `connectWithSslBundleAndOptionsMismatch`'s deliberately
            // mismatched cipher suite).
            // gc-common w15-d: `getCipherSuites` below is Java, and so is
            // every getter of the second block. The second block used to
            // take the socket and the parameters from `args` AFTER the
            // `getCipherSuites` upcall and pin those -- which roots the
            // vacated addresses, not the objects. Both are rooted here,
            // before any Java runs, and re-read at every use.
            let (this_arg, params_arg) = match (obj_arg(args, 0), args.get(1).copied()) {
                (Ok(this), Some(Value::Object(Some(params)))) => (this, params),
                _ => return Ok(None),
            };
            let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let this_h = scope.root(this_arg);
            let params_h = scope.root(params_arg);
            let params = scope.get(&params_h);
            if let Some(Value::Object(Some(protocols))) =
                scope.invoke_virtual(params, "getProtocols", "()[Ljava/lang/String;", &[])?
            {
                let this = scope.get(&this_h);
                crate::net_phase_e::ssl_socket_set_enabled_protocols(
                    &mut *scope,
                    &[Value::Object(Some(this)), Value::Object(Some(protocols))],
                )?;
            }

            {
                let this = scope.get(&this_h);
                let params = scope.get(&params_h);
                let tls_id = new13_resolve_tls_id(&*scope, this);
                if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
                    && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
                {
                    let mut ciphers = Vec::new();
                    // The array is `getCipherSuites`' own return: current.
                    let suites = scope.invoke_virtual(
                        params,
                        "getCipherSuites",
                        "()[Ljava/lang/String;",
                        &[],
                    );
                    if let Ok(Some(Value::Object(Some(arr)))) = suites {
                        let len = scope.array_length(arr);
                        for i in 0..len {
                            if let Value::Object(Some(s)) = scope.get_array_element(arr, i) {
                                if let Some(name) = scope.read_string(s) {
                                    ciphers.push(name);
                                }
                            }
                        }
                    }
                    if !ciphers.is_empty() {
                        let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
                        crate::t27_tls::set_pending_layered_socket_ciphers(pending_id, ciphers);
                    }
                }
            }
            // The rest of the object is CONFIGURATION and has to be kept
            // whether or not a handshake can still use it, because
            // `getSSLParameters()` must hand it back. Only the cipher list
            // above can still change a pending handshake; these three are
            // recorded so the round-trip is not lossy (`L6TlsParamSweep` row
            // 83 asked for exactly that and read `false null`).
            {
                // gen r4w3/rooting: every getter below is Java (a GC point).
                let params = scope.get(&params_h);
                let need = matches!(
                    scope.invoke_virtual(params, "getNeedClientAuth", "()Z", &[]),
                    Ok(Some(Value::Int(1)))
                );
                let params = scope.get(&params_h);
                let want = matches!(
                    scope.invoke_virtual(params, "getWantClientAuth", "()Z", &[]),
                    Ok(Some(Value::Int(1)))
                );
                let this = scope.get(&this_h);
                ssl_sock_auth_update(&*scope, this, |s| {
                    s.1 = i32::from(need);
                    s.2 = i32::from(want);
                });
                let params = scope.get(&params_h);
                let alg = match scope.invoke_virtual(
                    params,
                    "getEndpointIdentificationAlgorithm",
                    "()Ljava/lang/String;",
                    &[],
                ) {
                    Ok(Some(Value::Object(Some(s)))) => scope.read_string(s),
                    _ => None,
                };
                let this = scope.get(&this_h);
                let key = ssl_sock_key(&*scope, this);
                let mut table = ssl_sock_endpoint_alg().lock();
                match alg.filter(|a| !a.is_empty()) {
                    Some(a) => {
                        table.insert(key, a);
                    }
                    None => {
                        table.remove(&key);                    }                }            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // getSupportedCipherSuites/getEnabledCipherSuites/getSupportedProtocols/
    // getEnabledProtocols — same AbstractMethodError family as TC0622's
    // SSLSession buffer-size gap: the accessors above cover I/O and
    // lifecycle, but nothing registered these on `javax/net/ssl/SSLSocket`
    // itself, so `invokeinterface SSLSocket.getEnabledCipherSuites()`
    // resolved to the abstract interface declaration (no Code) and threw
    // `AbstractMethodError`. Mirrors the static suite/protocol lists already
    // used by `SSLEngineImpl` (t27_tls.rs) for consistency; the socket's
    // handshake already completed in `createSocket`/`accept`, so
    // enabled == supported here (matches the JDK default before any
    // `setEnabledCipherSuites` call — this synthetic socket has no
    // set-side storage, so `set*` below are accepted but not persisted).
    fn ssl_sock_supported_cipher_suites(
        ctx: &mut dyn NativeContext,
    ) -> Result<ObjectRef, MethodCallFailed> {
        // Single source of truth — see `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES`.
        // E42: the body moved to the module-level `jsse_supported_suite_name_array`
        // so the SSLEngine and SSLSocketFactory doors — which each carried
        // their own drifted inline copy of the list — share this one. This
        // wrapper stays because it is the name three registrations already use.
        jsse_supported_suite_name_array(ctx)
    }
    r.register(
        ssl_sock,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(ssl_sock_supported_cipher_suites(
                ctx,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(ssl_sock_supported_cipher_suites(
                ctx,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            // A socket from `createSocket(Socket wrapped, ...)`'s deferred
            // handshake honors this if called before the handshake actually
            // starts (real JSSE contract; `connectWithSslBundleAndOptionsMismatch`
            // relies on exactly this createSocket-then-narrow-ciphers
            // sequence to make the handshake genuinely fail on a
            // deliberately mismatched suite). A socket from any other path,
            // or one that's already handshaked, has no pending entry to
            // update — accepted but not persisted, same as before.
            let this = obj_arg(args, 0)?;
            let tls_id = new13_resolve_tls_id(ctx, this);
            if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
                && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
            {
                let mut ciphers = Vec::new();
                if let Some(Value::Object(Some(arr))) = args.get(1) {
                    let len = ctx.array_length(*arr);
                    for i in 0..len {
                        if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                            if let Some(name) = ctx.read_string(s) {
                                ciphers.push(name);
                            }
                        }
                    }
                }
                let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
                crate::t27_tls::set_pending_layered_socket_ciphers(pending_id, ciphers);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Single source of truth — see `t27_tls::SUPPORTED_PROTOCOL_NAMES`.
            // This copy was two elements while the engine's was three, so the
            // VM disagreed with itself about what it supports.
            let protos = crate::t27_tls::SUPPORTED_PROTOCOL_NAMES;
            // Typed and rooted (i6-L2) — see `jsse_supported_suite_name_array`.
            let arr = crate::t27_tls::materialize_java_string_array(ctx, protos);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // The protocol list `getEnabledProtocols()` and
    // `getSSLParameters().getProtocols()` BOTH answer.
    //
    // E42: measured equal on HotSpot for an unconnected socket and for a
    // version-pinned one (`scratchpad/e42/E42EnabledSets.java`), so they are
    // one answer through two doors. `getSSLParameters` used to have its own
    // copy that read the session raw and guessed `"TLSv1.3"` on a miss.
    fn ssl_sock_enabled_protocols(
        ctx: &mut dyn NativeContext,
        this: ObjectRef,
    ) -> Result<ObjectRef, MethodCallFailed> {
        if let Some(protocols) = crate::net_phase_e::sock_get(ctx, this).enabled_protocols {
            let names: Vec<&str> = protocols.iter().map(String::as_str).collect();
            return Ok(crate::t27_tls::materialize_java_string_array(ctx, &names));
        }
        jsse_enabled_protocol_array(ctx)
    }
    r.register(
        ssl_sock,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            Ok(Some(Value::Object(Some(ssl_sock_enabled_protocols(
                ctx, this,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // STUB-REMOVAL (wave 2): this was an unconditional no-op, so a caller
    // narrowing a socket to a single TLS version got no enforcement at all —
    // the security-relevant direction (an app disabling TLS 1.0/1.1, or pinning
    // 1.3) was silently discarded. `net_phase_e` registers the same
    // (class, method, descriptor) LATER and therefore wins in the real-JDK
    // build (see its own doc comment at `setEnabledProtocols`), but in
    // `--synthetic-jdk` mode `register_synthetic_overrides` runs AFTER
    // `register_essential_natives`, so THIS registration is the live one and
    // the restriction vanished. Mirror the `setEnabledCipherSuites` sibling
    // above: a socket from `createSocket(Socket wrapped, ...)` handshakes
    // lazily, so a restriction set beforehand still counts. A socket that has
    // already handshaked keeps the accept-and-discard behaviour (its version
    // is settled and rustls does not renegotiate).
    r.register(
        ssl_sock,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = new13_resolve_tls_id(ctx, this);
            if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
                && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
            {
                let mut protocols: Vec<String> = Vec::new();
                if let Some(Value::Object(Some(arr))) = args.get(1) {
                    let len = ctx.array_length(*arr);
                    for i in 0..len {
                        if let Value::Object(Some(s)) = ctx.get_array_element(*arr, i) {
                            if let Some(name) = ctx.read_string(s) {
                                protocols.push(name);
                            }
                        }
                    }
                }
                let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
                crate::t27_tls::set_pending_layered_socket_protocols(pending_id, protocols);            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getInputStream",
        "()Ljava/io/InputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut stream_scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let owner_h = stream_scope.root(this);
            let ctx = &mut *stream_scope;
            // NOT CONNECTED is a `SocketException`, before any handshake.
            //
            // MEASURED in BOTH modes against HotSpot 25.0.3+9
            // (`probes/Phase1Sweep.java`, the P1-C lane):
            //
            //   f.createSocket();  s.getOutputStream()
            //     HotSpot   SocketException      CratonVM  no-throw
            //
            // The probe's own CONTROL -- the same question on a plain
            // `java.net.Socket` -- already agreed, so this is SSL-specific:
            // `net_phase_e.rs`'s `java/net/Socket` pair screens `stream_id < 0`
            // and these two did not. The handshake starter was happy to mint an
            // id for a socket that had never been connected, so the caller got a
            // live-looking stream over nothing and the failure surfaced later,
            // somewhere else.
            //
            // `new13_socket_ever_connected` is the SAME predicate this file's
            // `isConnected` native answers with, so the two cannot disagree --
            // and `isConnected` already matched HotSpot on this receiver, which
            // is what says the information was present and simply unread.
            if !new13_socket_ever_connected(ctx, this) {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/net/SocketException",
                    "Socket is not connected",
                ));
            }
            // Real JSSE: the first I/O call implicitly starts the handshake
            // if one hasn't run yet — this socket may still be pending (see
            // `createSocket(Socket wrapped, ...)`'s doc comment).
            let fd_id = ensure_layered_handshake_started(ctx, this)?;
            if crate::nbflags().dbg_tls_sock || crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] SSLSocket.getInputStream sock={:?} tls_id={}",
                    this, fd_id
                );
            }
            // Return an InputStream that reads from the TLS fd.
            //
            // P1-C: `TLS_APP_IN_CLASS`, not the old
            // `javax/net/ssl/SSLSocketInputStream` — no supported image
            // declares that name, so under `--real-jdk` this line WAS the
            // `NoClassDefFoundError` that made HTTPS unusable. Read the block
            // on `TLS_APP_IN_CLASS` for the full argument and for why the
            // plain-socket precedent is followed on the layout but not on the
            // class name.
            let owner = stream_scope.get(&owner_h);
            let is = alloc_tls_stream(&mut *stream_scope, TLS_APP_IN_CLASS, fd_id, owner)?;
            // INVESTIGATED AND REVERTED (tls-handshake-enforcement-gap, doc
            // 21 — `TestSsl.testPost`): wrapping this in a real
            // `java.io.BufferedInputStream`, so that a single-byte `read()`
            // becomes bytecode over a Java `byte[]` instead of a native call.
            // That is the shape real JSSE's `SSLSocketImpl$AppInputStream`
            // has, and `testPost` reads 16 MiB one byte at a time on each of
            // 8 threads (~134 million reads), so it looked like the obvious
            // win. Measured: it made things WORSE — 368 s unwrapped vs >700 s
            // (test timeout) wrapped.
            //
            // Why, from a `--stack-dump-on-timeout=120` capture: all 8 client
            // threads sit in `BufferedInputStream.read`/`fill`/`getBufIfOpen`
            // with `blocked=false` and a DIFFERENT pc in each dump — real
            // progress, just too slow — while the Tomcat exec threads block in
            // `doWrite` waiting for them to drain. JDK 25's
            // `BufferedInputStream.read()` takes its `InternalLock` (or
            // `synchronized`) on EVERY call, so per byte it costs an AQS
            // lock/unlock plus interpreted bytecode, which on this VM is
            // dearer than the one native call it replaces.
            //
            // The per-byte cost here is the Java->native transition itself,
            // not the work behind it: a native-side readahead
            // (`servlet::s2_tls_fill_readahead`) removes the rustls and
            // registry work from every byte and only bought ~15%. Closing the
            // rest needs cheaper native dispatch, which is the pre-existing
            // throughput-wall work, not a TLS fix.
            Ok(Some(Value::Object(Some(is))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getOutputStream",
        "()Ljava/io/OutputStream;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let mut stream_scope = cratonvm_native_api::NativeHandleScope::new(ctx);
            let owner_h = stream_scope.root(this);
            let ctx = &mut *stream_scope;
            // NOT CONNECTED is a `SocketException`, before any handshake.
            //
            // MEASURED in BOTH modes against HotSpot 25.0.3+9
            // (`probes/Phase1Sweep.java`, the P1-C lane):
            //
            //   f.createSocket();  s.getOutputStream()
            //     HotSpot   SocketException      CratonVM  no-throw
            //
            // The probe's own CONTROL -- the same question on a plain
            // `java.net.Socket` -- already agreed, so this is SSL-specific:
            // `net_phase_e.rs`'s `java/net/Socket` pair screens `stream_id < 0`
            // and these two did not. The handshake starter was happy to mint an
            // id for a socket that had never been connected, so the caller got a
            // live-looking stream over nothing and the failure surfaced later,
            // somewhere else.
            //
            // `new13_socket_ever_connected` is the SAME predicate this file's
            // `isConnected` native answers with, so the two cannot disagree --
            // and `isConnected` already matched HotSpot on this receiver, which
            // is what says the information was present and simply unread.
            if !new13_socket_ever_connected(ctx, this) {
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/net/SocketException",
                    "Socket is not connected",
                ));
            }
            let fd_id = ensure_layered_handshake_started(ctx, this)?;
            if crate::nbflags().dbg_tls_sock {
                eprintln!(
                    "[dbg-tls-sock] thread={:?} getOutputStream sock={:?} tls_id={}",
                    std::thread::current().id(),
                    this,
                    fd_id
                );
            }
            // P1-C: see the `getInputStream` sibling above and the block on
            // `TLS_APP_IN_CLASS`. This is the half the strict-mode witness
            // actually reported (`NoClassDefFoundError:
            // javax/net/ssl/SSLSocketOutputStream`); the input half was the
            // same defect one call earlier and is fixed with it.
            let owner = stream_scope.get(&owner_h);
            let os = alloc_tls_stream(&mut *stream_scope, TLS_APP_OUT_CLASS, fd_id, owner)?;
            Ok(Some(Value::Object(Some(os))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ssl_sock, "close", "()V", |ctx, args| {
        let mut this = obj_arg(args, 0)?;
        // GUARD (P1-C, 2026-08-12) — this native can be handed a STREAM.
        //
        // `vm_exec.rs` and `invoke.rs` each carry an unconditional route:
        // any receiver whose class name `starts_with("sun/security/ssl/
        // SSLSocketImpl")` and whose (method, descriptor) is in a fixed
        // tuple list is dispatched to `javax/net/ssl/SSLSocket`'s native for
        // that pair. `("close", "()V")` is in that list, and the new stream
        // carriers — `sun/security/ssl/SSLSocketImpl$AppInputStream` and
        // `$AppOutputStream` — match the PREFIX. So `in.close()` on a stream
        // arrives here with the stream as `this`.
        //
        // Without this guard the body below would write `Value::Int` over
        // `NEW13_SOCK_TLSID`/`NEW13_SOCK_CLOSED` — slots 2 and 3, which on a
        // real `AppInputStream` are `appDataIsAvailable` (boolean) and
        // `readLock` (a `ReentrantLock` reference). That is the exact
        // wrong-type-into-a-reference-slot shape this campaign is fixing,
        // reached through a dispatch rule in a file this lane does not own.
        //
        // The right answer is also the documented one: closing a socket's
        // stream closes the socket, which is what `ssl_stream_close` does.
        // So detect and delegate rather than narrowing the predicate in
        // `vm_exec.rs` — that narrowing is nominated separately as the
        // durable fix, and this guard is correct with or without it.
        if is_tls_stream_carrier(ctx, this) {
            return ssl_stream_close(ctx, args);
        }
        // Drop any handshake-listener global roots first: they keep the
        // listener objects permanently reachable, and a closed socket will
        // never fire another event. Unconditional (not gated on `tls_id >= 0`
        // below) so a second close(), or a socket closed before it ever
        // handshaked, still releases them.
        new13_drop_handshake_listeners(ctx, this)?;
        // gc-common w16-d: and the roots of sockets that died unclosed.
        release_dead_socket_listener_roots(ctx);
        let tls_id = new13_resolve_tls_id(ctx, this);
        // Connected but never used (H2 `TcpServer.isRunning()` is exactly
        // this): there is no TLS stream to close, only parked state to free.
        drop_pending_connect_socket_if_any(tls_id);
        // gc-common w16-d handoff: a layered socket closed before its
        // handshake releases its parked connection too.
        if (crate::servlet::PENDING_LAYERED_SOCK_ID_BASE..crate::servlet::RUSTLS_SOCK_ID_BASE)
            .contains(&tls_id)
        {
            crate::t27_tls::drop_pending_layered_socket(
                tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE,
            );
        }
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} JAVA_CALLED Socket.close() tls_id={}",
                std::thread::current().id(),
                tls_id
            );
        }
        if tls_id >= 0 {
            // Idempotent — s2_tls_close tolerates an unknown id. The TCP
            // half-close inside shutdown() also flushes any pending TLS
            // close_notify, preserving RFC 8446 §6.1 semantics.
            let pin = ctx.pin_native_root(this);
            ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let _ = crate::servlet::s2_tls_close(tls_id);
            ctx.end_blocking_region();
            this = ctx.read_native_pin(pin, this);
            ctx.unpin_native_roots(pin);
            // Overwrite the id so a second close() is a no-op.
            sock_set_field(&*ctx, this, NEW13_SOCK_TLSID, Value::Int(-1));
            // Mark the session as invalidated so isValid() returns false.
            if let Value::Object(Some(session)) = sock_field(&*ctx, this, NEW13_SOCK_SESSION) {
                if crate::t27_tls::session_shape_width(&*ctx, session) > NEW13_SESS_TLSID {
                    crate::t27_tls::sess_set(&*ctx, session, NEW13_SESS_TLSID, Value::Int(-1));
                }
            }
        }
        // Keep net_phase_e's side table (if this socket was built through
        // its createSocket(String,int) — see new13_resolve_tls_id) in sync,
        // so any other code path that consults it also observes closed.
        crate::net_phase_e::sock_mark_closed_for_upcall(ctx, this);
        sock_set_field(&*ctx, this, NEW13_SOCK_CLOSED, Value::Int(1));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = match sock_field(&*ctx, this, NEW13_SOCK_CLOSED) {
            Value::Int(c) => c != 0,
            _ => crate::net_phase_e::sock_is_closed_for_upcall(ctx, this),
        };
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} isClosed sock={:?} -> {}",
                std::thread::current().id(),
                this,
                closed
            );
        }
        Ok(Some(Value::Int(if closed { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // E42 (E31-1 NOMINATION 6): this answered `!isClosed()`, and a positive
    // predicate implemented as the negation of a different one is wrong at both
    // ends — a never-connected socket is not closed, and a closed socket was
    // still once connected. Measured on HotSpot, both directions, in
    // `new13_socket_ever_connected`'s doc comment; the JDK keeps `CONNECTED`
    // and `CLOSED` in separate bits and `close()` never touches the first.
    //
    // This is the ONE check `regression-suite/src/RSslNullSession.java`
    // currently reaches (`ck("socket.isConnected", s.isConnected(),
    // Boolean.FALSE)`, DOOR 1 line 1), and it was RED — invisibly, because the
    // vector aborts on the next line and never prints a summary.
    //
    // The sibling `isClosed()` immediately above is CORRECT and deliberately
    // unchanged: it reads the closed flag, which is what it is asking about.
    r.register(ssl_sock, "isConnected", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let connected = new13_socket_ever_connected(ctx, this);
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} isConnected sock={:?} -> {}",
                std::thread::current().id(),
                this,
                connected
            );
        }
        Ok(Some(Value::Int(if connected { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // E42: `isBound()` had NO registration on this class, so the real
    // `java.net.Socket.isBound()` bytecode ran and read the `state` word out of
    // a synthetic object whose slots this file writes `Int`s into — the exact
    // "undefined, and in practice non-deterministically truthy roughly one run
    // in three" shape `net_phase_e` recorded when `isInputShutdown` had the
    // same gap. Measured (`scratchpad/e42/E42SocketPredicates.java`): `false`
    // on a fresh socket, `true` once connected, and STILL `true` after
    // `close()` — a latch, like `isConnected()`.
    //
    // On this VM's `SSLSocket` surface the two latches coincide, and that is a
    // statement about the surface rather than about `java.net.Socket`: there is
    // no `bind` registration on this class, so a caller cannot reach the
    // bound-but-not-connected state HotSpot's arm H shows (a plain
    // `new Socket()` + `bind()`, which answers `isBound()=true`
    // `isConnected()=false`). If `SSLSocket.bind` is ever registered, this must
    // gain its own flag rather than keep sharing one.
    r.register(ssl_sock, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bound = new13_socket_ever_connected(ctx, this);
        Ok(Some(Value::Int(if bound { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    // `java.net.Socket` has the same registrations, but a real-JDK
    // `SSLSocket` receiver does not reliably inherit them through the native
    // dispatch lookup.  Falling through to Socket's bytecode reads the real
    // `Socket.impl` / `shutIn` fields from this synthetic overlay and can
    // report a live TLS connection as input-shut.  HttpComponents 5.4 checks
    // `isInputShutdown()` immediately before every request-body write and
    // turns that false positive into `ConnectionClosedException`.
    //
    // E42 — the SAME conflation as `isConnected()` above, in the same file, and
    // the correct implementation already existed one module away.
    //
    // "There is no independent half-close state for a rustls SSLSocket: the
    // only supported shutdown operation is close(), which marks the shared
    // side-table entry closed. Use that authoritative state for both
    // directions" — that is what stood here, and both clauses are false.
    // `net_phase_e`'s `SockSide` has carried dedicated `input_shutdown` /
    // `output_shutdown` flags all along; its own `java/net/Socket
    // .shutdownInput()`/`shutdownOutput()` natives set them, and its own
    // `isInputShutdown`/`isOutputShutdown` READ them
    // (`net_phase_e.rs`, `r.register(sock, "isInputShutdown", ...)` →
    // `sock_get(ctx, this).input_shutdown`). These two copies were the drifted
    // twins that asked `closed` instead — the same "the right helper exists and
    // only some call sites use it" shape this directory keeps recording.
    //
    // Measured, HotSpot 25.0.3+9-LTS, three byte-identical runs
    // (`scratchpad/e42/E42SocketPredicates.java`):
    //
    //     fresh socket                    isInputShutdown=false isOutputShutdown=false
    //     closed, never shut down         isInputShutdown=false isOutputShutdown=false
    //     connected + shutdownOutput()    isInputShutdown=false isOutputShutdown=true
    //     that socket after close()       isInputShutdown=false isOutputShutdown=true
    //
    // i.e. `close()` sets NEITHER. `SHUT_IN`/`SHUT_OUT` are their own bits
    // (`jdk25src/java.base/java/net/Socket.java:119-120`), written only by
    // `shutdownInput`/`shutdownOutput`.
    //
    // **The consumer the old spelling was written for is unaffected.** Apache
    // HttpClient5's `DefaultBHttpClientConnection$1.checkTLS()` calls
    // `isInputShutdown()` before every write on a LIVE socket; that socket is
    // not closed and has not been shut down, so it answered `false` before and
    // answers `false` now. What changes is a CLOSED socket, which stops
    // claiming a half-close that never happened — HotSpot's answer.
    r.register(ssl_sock, "isInputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            crate::net_phase_e::sock_get(ctx, this).input_shutdown,
        )))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "isOutputShutdown", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            crate::net_phase_e::sock_get(ctx, this).output_shutdown,
        )))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "getPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // The raw slot is written by the sockets `new13_*` built; a socket from
        // `createSocket(String,int)` (net_phase_e) records its peer in the side table only.
        match sock_field(&*ctx, this, NEW13_SOCK_PORT) {
            v @ Value::Int(port) if port > 0 => Ok(Some(v)),
            _ => Ok(Some(Value::Int(
                crate::net_phase_e::sock_get(ctx, this).port,
            ))),
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    // STUB-REMOVAL (wave 2): `getSoTimeout` returned a hard-coded 0 ("no
    // timeout") and `setSoTimeout` validated its argument then threw it away,
    // so a caller that configured a read timeout and read it back was told the
    // socket blocks forever — and the underlying `TcpStream` really did. Apply
    // the timeout to the live stream (exactly what `net_phase_e`'s
    // `java/net/Socket.setSoTimeout` does for the plain-socket case, which this
    // more-specific `SSLSocket` registration intercepts ahead of) and report
    // back what is actually configured.
    r.register(ssl_sock, "getSoTimeout", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if crate::net_phase_e::sock_get(ctx, this).closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        let configured = crate::net_phase_e::sock_get(ctx, this).read_timeout_ms;
        if configured != 0 {
            return Ok(Some(Value::Int(configured)));
        }
        let tls_id = new13_resolve_tls_id(ctx, this);
        if tls_id >= 0 {
            let reg = crate::servlet::s2_registry().lock();
            let timeout = if let Some(stream) = reg.streams.get(&tls_id) {
                stream.read_timeout().ok().flatten()
            } else if let Some(raw) = reg.tls_streams.get(&tls_id).and_then(|e| e.raw.as_ref()) {
                raw.read_timeout().ok().flatten()
            } else {
                None
            };
            if let Some(d) = timeout {
                return Ok(Some(Value::Int(d.as_millis() as i32)));
            }
        }
        Ok(Some(Value::Int(0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "setSoTimeout", "(I)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let timeout = args.get(1).and_then(Value::as_int).unwrap_or(0);
        if timeout < 0 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "timeout can't be negative".into(),
            }
            .into());
        }
        if crate::net_phase_e::sock_get(ctx, this).closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        let tls_id = new13_resolve_tls_id(ctx, this);
        let d = if timeout == 0 {
            None
        } else {
            Some(std::time::Duration::from_millis(timeout as u64))
        };
        if let Some(result) =
            crate::net_phase_e::re1_with_raw_stream(tls_id, |tcp| tcp.set_read_timeout(d))
        {
            result.map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        }
        crate::net_phase_e::sock_set(ctx, this, |s| s.read_timeout_ms = timeout);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ssl_sock,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let host = match sock_field(&*ctx, this, NEW13_SOCK_HOST) {
                Value::Object(Some(host)) => ctx.read_string(host).unwrap_or_default(),
                _ => crate::net_phase_e::sock_get(ctx, this).host,
            };
            if host.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            let address = crate::net_phase_e::alloc_inet_address_for_input(ctx, &host, &host)?;
            Ok(Some(Value::Object(Some(address))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getRemoteSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Defensive: InetSocketAddress(String,int)'s native ctor NPEs on
            // a null host (see the java/net/Socket sibling fix in
            // phases_early.rs) -- every known writer of NEW13_SOCK_HOST sets
            // a non-null placeholder, but never pass a raw possibly-unset
            // field straight through to a ctor that requires non-null.
            // The port is read BEFORE the placeholder String's allocation,
            // which can move the socket (gc-common w15-d).
            let port = sock_field(&*ctx, this, NEW13_SOCK_PORT);
            let host = match sock_field(&*ctx, this, NEW13_SOCK_HOST) {
                h @ Value::Object(Some(_)) => h,
                _ => Value::Object(Some(ctx.create_string("0.0.0.0"))),
            };
            ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/lang/String;I)V",
                &[host, port],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // STUB-REMOVAL (wave 2): was a hard-coded 0. `net_phase_e`'s socket side
    // table records the real bound local port at create/connect time
    // (`sock_set_for_create_with_local_port`); read it, and only fall back to 0
    // (the JDK's "not bound" answer) when nothing was recorded.
    r.register(ssl_sock, "getTcpNoDelay", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let side = crate::net_phase_e::sock_get(ctx, this);
        if side.closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        let on = if false {
            side.keep_alive
        } else {
            side.tcp_no_delay
        };
        Ok(Some(Value::Int(i32::from(on))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "setTcpNoDelay", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let side = crate::net_phase_e::sock_get(ctx, this);
        if side.closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        let on = args.get(1).and_then(Value::as_int).unwrap_or(0) != 0;
        let sid = new13_resolve_tls_id(ctx, this);
        if let Some(result) = crate::net_phase_e::re1_with_raw_stream(sid, |tcp| {
            if false {
                socket2::SockRef::from(tcp).set_keepalive(on)
            } else {
                tcp.set_nodelay(on)
            }
        }) {
            result.map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        }
        crate::net_phase_e::sock_set(ctx, this, |s| {
            if false {
                s.keep_alive = on;            } else {
                s.tcp_no_delay = on;            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "getKeepAlive", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let side = crate::net_phase_e::sock_get(ctx, this);
        if side.closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        let on = if true {
            side.keep_alive
        } else {
            side.tcp_no_delay
        };
        Ok(Some(Value::Int(i32::from(on))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "setKeepAlive", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let side = crate::net_phase_e::sock_get(ctx, this);
        if side.closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        let on = args.get(1).and_then(Value::as_int).unwrap_or(0) != 0;
        let sid = new13_resolve_tls_id(ctx, this);
        if let Some(result) = crate::net_phase_e::re1_with_raw_stream(sid, |tcp| {
            if true {
                socket2::SockRef::from(tcp).set_keepalive(on)
            } else {
                tcp.set_nodelay(on)
            }
        }) {
            result.map_err(|e| RuntimeError::IOException {
                message: e.to_string(),
            })?;
        }
        crate::net_phase_e::sock_set(ctx, this, |s| {
            if true {
                s.keep_alive = on;            } else {
                s.tcp_no_delay = on;            }        });
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_sock, "getLocalPort", "()I", |ctx, args| {
        let side = crate::net_phase_e::sock_get(ctx, obj_arg(args, 0)?);
        Ok(Some(Value::Int(if side.local_port == 0 {
            -1
        } else {
            side.local_port
        })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ssl_sock,
        "getLocalAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let sid = new13_resolve_tls_id(ctx, this);
            let host = crate::net_phase_e::re1_with_raw_stream(sid, |tcp| {
                tcp.local_addr().ok().map(|a| a.ip().to_string())
            })
            .flatten()
            .unwrap_or_else(|| "0.0.0.0".into());
            let address = crate::net_phase_e::alloc_inet_address_unnamed(ctx, &host)?;
            Ok(Some(Value::Object(Some(address))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_sock,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let side = crate::net_phase_e::sock_get(ctx, this);
            if side.local_port == 0 {
                return Ok(Some(Value::Object(None)));
            }
            let sid = new13_resolve_tls_id(ctx, this);
            let host = crate::net_phase_e::re1_with_raw_stream(sid, |tcp| {
                tcp.local_addr().ok().map(|a| a.ip().to_string())
            })
            .flatten()
            .unwrap_or_else(|| "0.0.0.0".into());
            let addr = crate::net_phase_e::alloc_inet_address_unnamed(ctx, &host)?;
            ctx.new_object_initialized(
                "java/net/InetSocketAddress",
                "(Ljava/net/InetAddress;I)V",
                &[Value::Object(Some(addr)), Value::Int(side.local_port)],
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // NEW-13: the TLS socket's InputStream — reads from a `s2_registry` TLS
    // stream identified by the tls_id in the carrier's appended private slot
    // (see `alloc_tls_stream` / `tls_stream_id`). A -1 id or short-read of 0
    // maps to Java EOF (-1) per InputStream.read semantics.
    //
    // P1-C (2026-08-12): the carrier class changed from the never-declared
    // `javax/net/ssl/SSLSocketInputStream` to the image's own
    // `sun/security/ssl/SSLSocketImpl$AppInputStream`, and every body below
    // therefore stopped reading slot 0 — on a real layout slot 0 is
    // `oneByte`, a `byte[]`. Both class names are registered: the new one is
    // what this file's `getInputStream` hands out, the old one is still minted
    // by `net_phase_e`'s layered-socket branch and is pinned by
    // `scripts/baselines/jdk-only-gated-never-delete.tsv`. See the
    // `TLS_APP_IN_CLASS` block for the whole argument.
    //
    // PERF NOTE (testssl-testpost bulk TLS, 2026-08-05): this native's own body
    // is NOT what makes a byte-at-a-time reader slow. Measured against
    // `SSLSocketOutputStream.flush()` — a registered no-op native on the same
    // receiver, so the difference is the body and nothing else — one `read()`
    // costs 814 ns at 1 thread, of which 549 ns is the Java->native transition
    // and only ~265 ns is everything this body does. Counters over a full
    // `testPost` confirmed the two candidate slow spots are absent: the private
    // slot read hits every time (16,777,216 of 16,777,216 calls — the
    // side-table fallback never fires), and the readahead does exactly one real
    // socket read per 16 KiB (1024 refills, avg 16384 bytes). What remained of
    // the body was the descriptor lookup behind `get_field`; see
    // `vm_exec::resolve_field_descriptor_byte_cached`'s stub note.
    // `tls_stream_id` keeps that shape: one `object_num_fields` header read
    // plus the same single `get_field`, and NOT a class lookup by name.
    fn ssl_stream_read_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ssl_stream_owner_closed(ctx, this) {
            return Ok(Some(Value::Int(-1)));
        }

        let tls_id = tls_stream_id(ctx, this);
        if tls_id < 0 {
            return Ok(Some(Value::Int(-1)));
        }
        // FIX (tls-handshake-enforcement-gap, doc 21 — TestSsl hang): serve
        // from the shared plaintext readahead when possible. Reading a single
        // byte through the full native path (blocking-region bracket +
        // registry lock + per-call rustls plumbing) costs microseconds, and a
        // caller that reads a multi-megabyte body a byte at a time —
        // `TestSsl.testPost` reads 16 MiB per thread on 8 threads, ~134
        // million calls — turned that into minutes. Real JSSE serves these
        // out of a plain Java `byte[]`. See `servlet::s2_tls_fill_readahead`
        // for why the buffer is keyed by stream id.
        if let Some(b) = crate::servlet::s2_tls_pop_buffered_byte(tls_id) {
            return Ok(Some(Value::Int(b as i32)));
        }
        // STW-COOPERATION (tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED):
        // the refill does a genuine OS-level blocking socket operation.
        // Without the blocked-region bracket this thread stays counted as a
        // cooperative mutator that can never reach a safepoint poll, so a
        // concurrent STW (GC / cross-thread JIT takeover) waits on it forever
        // — and the bytes it waits for are produced by a peer thread in this
        // same process (Tomcat's NioEndpoint SocketProcessor) which DOES stop
        // at the barrier: a mutual deadlock, observed as `rounds=64 pending=1
        // taken=0` repeating with no further progress. Same bug shape as the
        // `net_phase_e` HttpClient and S2 selector fixes; see
        // `keycloak-model-stw-takeover-hang-eventloopgroup-shutdown-FIXED.md`.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let filled = crate::servlet::s2_tls_fill_readahead(tls_id);
        ctx.end_blocking_region();
        match filled {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(_) => Ok(Some(Value::Int(
                crate::servlet::s2_tls_pop_buffered_byte(tls_id)
                    .map(|b| b as i32)
                    .unwrap_or(-1),
            ))),
            Err(e) => Err(tls_io_failure(ctx, tls_id, e)),
        }
    }
    fn ssl_stream_read_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let tls_id = tls_stream_id(ctx, this);
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    // JDK 25 `SSLSocketImpl.AppInputStream.read` (wave 100 L7).
                    message: Some("the target buffer is null".into()),
                }
                .into());
            }
        };
        // JDK 25 `SSLSocketImpl.AppInputStream.read` / `AppOutputStream.write`:
        // `IndexOutOfBoundsException("buffer length: n, offset; o, bytes to
        // read:l")` (the same text for both, typo included), checked on the
        // signed `int`s (wave 100 L7; this was an AIOOBE "Array index out of
        // range: n", and a negative offset or length was cast to `usize`).
        let off_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len_i = ctx.array_length(arr) as i64;
        if off_i < 0 || len_i < 0 || i64::from(len_i) > arr_len_i - i64::from(off_i) {
            return Err(RuntimeError::ioobe(format!(
                "buffer length: {arr_len_i}, offset; {off_i}, bytes to read:{len_i}"
            ))
            .into());
        }
        let off = off_i as usize;
        let len = len_i as usize;
        // Bounds check against the target array length so a bad caller can't
        // poison indices in `set_array_element`.
        let arr_len = ctx.array_length(arr);
        if off.saturating_add(len) > arr_len {
            return Err(RuntimeError::aioobe_index_only(off.saturating_add(len) as i32).into());
        }
        if len == 0 {
            return Ok(Some(Value::Int(0)));
        }
        if tls_id < 0 || ssl_stream_owner_closed(ctx, this) {
            return Ok(Some(Value::Int(-1)));
        }
        let mut buf = vec![0u8; len];
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `SSLSocketInputStream.read()I` above.
        // `arr` is written AFTER the block, so it must survive a moving
        // collection that runs while this thread is parked — pin it.
        let arr_pin = ctx.pin_native_root(arr);
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let read_result = crate::servlet::s2_tls_read(tls_id, &mut buf);
        ctx.end_blocking_region();
        let arr = ctx.read_native_pin(arr_pin, arr);
        ctx.unpin_native_roots(arr_pin);
        match read_result {
            Ok(0) => Ok(Some(Value::Int(-1))),
            Ok(n) => {
                // PERF (testssl-testpost bulk TLS): one bulk copy instead of one
                // virtual `set_array_element` (plus a `Value` box) per byte. The
                // range was bounds-checked against `arr_len` above, so a `false`
                // here can only mean the pinned array came back smaller — report
                // that rather than silently claim bytes that never landed.
                if !ctx.write_byte_array_from(arr, off, &buf[..n]) {
                    return Err(RuntimeError::IOException {
                        message: "SSLSocketInputStream.read: destination array shrank".into(),
                    }
                    .into());
                }
                Ok(Some(Value::Int(n as i32)))
            }
            Err(e) => Err(tls_io_failure(ctx, tls_id, e)),
        }
    }
    fn ssl_stream_available(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        // native-tls does not expose a non-blocking peek, so match the
        // reference JDK behaviour of reporting 0 bytes readable without
        // blocking — EXCEPT for plaintext already pulled off the socket into
        // this stream's readahead (see `servlet::s2_tls_fill_readahead`),
        // which is readable without blocking by definition and must be
        // reported, or a caller that loops on `available()` would stall on
        // bytes it has effectively already received.
        let this = obj_arg(args, 0)?;
        let tls_id = tls_stream_id(ctx, this);
        if tls_id < 0 {
            return Ok(Some(Value::Int(0)));
        }
        Ok(Some(Value::Int(
            crate::servlet::s2_tls_buffered_len(tls_id) as i32,
        )))
    }
    // MUST be registered, and this is new with P1-C. `java.io.InputStream`'s
    // own `skip(long)` would have been fine — it reads into a temporary array
    // through `read([BII)` — but the real `SSLSocketImpl$AppInputStream`
    // DECLARES `skip(long)`, and its body takes `this.readLock` and reads
    // `this.buffer`. Both are `null` on a carrier this file allocated without
    // running the real constructor, so leaving `skip` unregistered would turn
    // `in.skip(n)` (every HTTP client that discards a response body) into an
    // NPE inside java.base. This is the general hazard of moving onto a real
    // receiver: every concrete method the real class declares must either be
    // shadowed or be safe against an all-null layout. `available`, `read()`,
    // `read([BII)` and `close` are shadowed above/below; `checkEOF`,
    // `deplete` and `readLockedDeplete` are private and reachable only from
    // those; `read([B)`, `mark`, `reset`, `markSupported`, `readAllBytes`,
    // `readNBytes` and `transferTo` are NOT declared by `AppInputStream`, so
    // they run `java.io.InputStream`'s bodies over the shadowed
    // `read([BII)` — which is what they do on HotSpot too.
    fn ssl_stream_skip(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let tls_id = tls_stream_id(ctx, this);
        let n = args.get(1).and_then(|v| v.as_long()).unwrap_or(0);
        if tls_id < 0 || n <= 0 {
            return Ok(Some(Value::Long(0)));
        }
        // Drain the readahead first — free, and the common case for a small
        // skip over an already-buffered body.
        let mut skipped: i64 = 0;
        while skipped < n {
            match crate::servlet::s2_tls_pop_buffered_byte(tls_id) {
                Some(_) => skipped += 1,
                None => break,
            }
        }
        if skipped >= n {
            return Ok(Some(Value::Long(skipped)));
        }
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `read()I` above. One region around the whole loop, and the error is
        // carried out rather than returned from inside it.
        let mut scratch = vec![0u8; 8192];
        let mut err: Option<String> = None;
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        while skipped < n {
            let want = ((n - skipped) as usize).min(scratch.len());
            match crate::servlet::s2_tls_read(tls_id, &mut scratch[..want]) {
                Ok(0) => break,
                Ok(read) => skipped += read as i64,
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        ctx.end_blocking_region();
        if let Some(message) = err {
            // Bytes already skipped are gone from the stream either way; report
            // the failure rather than claiming a clean partial skip, which is
            // what the real body does (it propagates the IOException).
            return Err(RuntimeError::IOException { message }.into());
        }
        Ok(Some(Value::Long(skipped)))
    }
    // STUB-REMOVAL (wave 2): `close()` on either of the two TLS socket streams
    // was an unconditional no-op, so `sslSocket.getInputStream().close()` (and
    // the output-stream sibling) left the TLS session and its OS socket open —
    // one leaked fd and one live TLS connection per caller that closes the
    // stream instead of the socket. The JDK contract for a socket's own streams
    // is the opposite: "Closing the returned InputStream will close the
    // associated socket" (`java.net.Socket.getInputStream`). Mirror
    // `SSLSocket.close()` above: shut the TLS stream down (which flushes
    // close_notify), stamp the id field to -1 so a second close is a no-op and
    // any later read reports EOF rather than reusing a recycled id, and keep
    // `net_phase_e`'s side table in sync.
    //
    // NOT CHANGED HERE, and deliberately: this closes the TLS stream, it does
    // not add or alter any asynchronous/interruptible close behaviour. The
    // wake-a-parked-reader question is a different lane's (W7-61 item 2,
    // W2-2) and nothing in P1-C touches it.
    fn ssl_stream_owner_closed(ctx: &dyn NativeContext, stream: ObjectRef) -> bool {
        if crate::net_phase_e::sock_is_closed_for_upcall(ctx, stream) {
            return true;
        }
        match ctx.get_field_by_name(stream, "this$0") {
            Value::Object(Some(owner)) => crate::net_phase_e::sock_is_closed_for_upcall(ctx, owner),
            _ => false,
        }
    }

    fn ssl_stream_close(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let stream_h = scope.root(this);
        if let Value::Object(Some(owner)) = scope.get_field_by_name(this, "this$0") {
            scope.invoke_virtual(owner, "close", "()V", &[])?;
            let stream = scope.get(&stream_h);
            tls_stream_clear_id(&mut *scope, stream);
            crate::net_phase_e::sock_mark_closed_for_upcall(&mut *scope, stream);
            return Ok(None);
        }
        let tls_id = tls_stream_id(&mut *scope, this);
        if tls_id >= 0 {
            scope.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
            let _ = crate::servlet::s2_tls_close(tls_id);
            scope.end_blocking_region();
            let stream = scope.get(&stream_h);
            tls_stream_clear_id(&mut *scope, stream);
        }
        let stream = scope.get(&stream_h);
        crate::net_phase_e::sock_mark_closed_for_upcall(&mut *scope, stream);
        Ok(None)
    }

    // NEW-13: the TLS socket's OutputStream — writes to `s2_registry` TLS
    // stream. Same P1-C carrier change as the input half above.
    fn ssl_stream_write_one(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        if ssl_stream_owner_closed(ctx, this) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Connection or outbound has closed",
            ));
        }

        let tls_id = tls_stream_id(ctx, this);
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} SSLSocketOutputStream.write(int) tls_id={}",
                std::thread::current().id(),
                tls_id
            );
        }
        if tls_id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLSocketOutputStream.write: stream is closed".into(),
            }
            .into());
        }
        let b = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) as u8;
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `SSLSocketInputStream.read()I` above.
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let write_result = crate::servlet::s2_tls_write(tls_id, &[b]);
        ctx.end_blocking_region();
        if let Err(e) = write_result {
            return Err(tls_io_failure(ctx, tls_id, e));
        }
        Ok(None)
    }
    fn ssl_stream_write_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
        let this = obj_arg(args, 0)?;
        let tls_id = tls_stream_id(ctx, this);
        let len_arg = args.get(3).and_then(|v| v.as_int()).unwrap_or(-1);
        if crate::nbflags().dbg_tls_sock {
            eprintln!(
                "[dbg-tls-sock] thread={:?} SSLSocketOutputStream.write([BII) tls_id={} len={}",
                std::thread::current().id(),
                tls_id,
                len_arg
            );
        }
        let arr = match args.get(1) {
            Some(Value::Object(Some(a))) => *a,
            _ => {
                return Err(RuntimeError::NullPointerException {
                    // JDK 25 `SSLSocketImpl.AppOutputStream.write` (wave 100 L7).
                    message: Some("the source buffer is null".into()),
                }
                .into());
            }
        };
        // JDK 25 `SSLSocketImpl.AppInputStream.read` / `AppOutputStream.write`:
        // `IndexOutOfBoundsException("buffer length: n, offset; o, bytes to
        // read:l")` (the same text for both, typo included), checked on the
        // signed `int`s (wave 100 L7; this was an AIOOBE "Array index out of
        // range: n", and a negative offset or length was cast to `usize`).
        let off_i = args.get(2).and_then(|v| v.as_int()).unwrap_or(0);
        let len_i = args.get(3).and_then(|v| v.as_int()).unwrap_or(0);
        let arr_len_i = ctx.array_length(arr) as i64;
        if off_i < 0 || len_i < 0 || i64::from(len_i) > arr_len_i - i64::from(off_i) {
            return Err(RuntimeError::ioobe(format!(
                "buffer length: {arr_len_i}, offset; {off_i}, bytes to read:{len_i}"
            ))
            .into());
        }
        let off = off_i as usize;
        let len = len_i as usize;
        let arr_len = ctx.array_length(arr);
        if off.saturating_add(len) > arr_len {
            return Err(RuntimeError::aioobe_index_only(off.saturating_add(len) as i32).into());
        }
        if len == 0 {
            return Ok(None);
        }
        if ssl_stream_owner_closed(ctx, this) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Connection or outbound has closed",
            ));
        }
        if tls_id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLSocketOutputStream.write: stream is closed".into(),
            }
            .into());
        }
        // PERF (testssl-testpost bulk TLS): one bulk copy instead of one virtual
        // `get_array_element` per byte. `TestSsl.testPost` alone pushes 16 MiB
        // per thread through here in 128 KiB blocks.
        let mut buf = vec![0u8; len];
        let copied = ctx.read_byte_array_into(arr, off, &mut buf);
        // The range was bounds-checked against `arr_len` above, so a short copy
        // means the array is not the byte[] this signature promises. Writing the
        // zero tail would put bytes on the wire the caller never supplied.
        buf.truncate(copied);
        // Drain the write — TlsStream::write may return short writes under
        // pressure, which callers would otherwise interpret as silent data
        // loss. Loop until the whole buffer has been accepted.
        let mut written = 0usize;
        // STW-COOPERATION: blocking socket I/O — see the full rationale on
        // `SSLSocketInputStream.read()I` above.
        // The whole drain loop goes in ONE blocked region, and the error
        // is carried out instead of returned from inside it — an early
        // `return` between begin/end would leave this thread permanently
        // marked blocked (the mirror-image failure: a live mutator the
        // barrier stops waiting for).
        ctx.begin_blocking_region_as(cratonvm_native_api::BlockKind::InNative);
        let mut write_err: Option<std::io::Error> = None;
        while written < buf.len() {
            match crate::servlet::s2_tls_write(tls_id, &buf[written..]) {
                Ok(0) => {
                    write_err = Some(std::io::Error::new(
                        std::io::ErrorKind::WriteZero,
                        "SSLSocketOutputStream.write: peer closed",
                    ));
                    break;
                }
                Ok(n) => written += n,
                Err(e) => {
                    write_err = Some(e);
                    break;
                }
            }
        }
        ctx.end_blocking_region();
        if let Some(e) = write_err {
            return Err(tls_io_failure(ctx, tls_id, e));
        }
        Ok(None)
    }

    // The registrations, on BOTH carriers.
    //
    // `TLS_APP_IN_CLASS` / `TLS_APP_OUT_CLASS` are what `getInputStream` /
    // `getOutputStream` now hand out. `TLS_LEGACY_IN_CLASS` /
    // `TLS_LEGACY_OUT_CLASS` are kept because `net_phase_e`'s own
    // `java/net/Socket.getInputStream()` still mints them for a layered
    // `SSLSocket` (net_phase_e.rs ~5273/5296, not this lane's file) and
    // because `scripts/baselines/jdk-only-gated-never-delete.tsv` pins all
    // eight legacy rows by name. Nothing else registers these triples on
    // either receiver, so there is no last-write-wins question here — grepped
    // across the workspace for `SSLSocketImpl$App` (this file only) and for
    // the two legacy names (this file and the two `net_phase_e` mint sites,
    // which register nothing).
    //
    // Kind: both receivers inherit `register_p68_ssl`'s ambient category,
    // which the frozen kind map records as `bridge` for this block. The two
    // legacy names are re-tagged `SyntheticStub` by
    // `no_image_receiver::NO_IMAGE_JDK_RECEIVERS` (no image declares them) and
    // so stay dropped under `--real-jdk` — correctly, since strict mode also
    // refuses to mint them. The two new names are on none of those tables and
    // name classes every image DOES declare, so they survive strict mode,
    // which is the whole of this fix.
    for is_cls in [TLS_APP_IN_CLASS, TLS_LEGACY_IN_CLASS] {
        r.register(is_cls, "read", "()I", ssl_stream_read_one, cratonvm_native_api::NativeKind::Bridge);
        r.register(is_cls, "read", "([BII)I", ssl_stream_read_bytes, cratonvm_native_api::NativeKind::Bridge);
        r.register(is_cls, "available", "()I", ssl_stream_available, cratonvm_native_api::NativeKind::Bridge);
        r.register(is_cls, "skip", "(J)J", ssl_stream_skip, cratonvm_native_api::NativeKind::Bridge);
        r.register(is_cls, "close", "()V", ssl_stream_close, cratonvm_native_api::NativeKind::Bridge);
    }
    for os_cls in [TLS_APP_OUT_CLASS, TLS_LEGACY_OUT_CLASS] {
        r.register(os_cls, "write", "(I)V", ssl_stream_write_one, cratonvm_native_api::NativeKind::Bridge);
        r.register(os_cls, "write", "([BII)V", ssl_stream_write_bytes, cratonvm_native_api::NativeKind::Bridge);
        // KEEP (genuinely empty): every `write` above hands its bytes straight
        // to `servlet::s2_tls_write`, which drives the underlying `TlsStream`
        // and its `TcpStream` synchronously — there is no Java- or Rust-side
        // buffer between the caller and the socket, so there is nothing for
        // `flush()` to push. VERIFIED against jdk-25 (`javap -p
        // sun.security.ssl.SSLSocketImpl$AppOutputStream`): the class declares
        // no `flush` at all, so the real call inherits
        // `java.io.OutputStream.flush()`, whose body is empty. The real
        // behaviour is a no-op, not merely equivalent to one — which is also
        // why this registration is safe on the real carrier.
        r.register(os_cls, "flush", "()V", |_ctx, _args| Ok(None), cratonvm_native_api::NativeKind::Bridge);
        // See `ssl_stream_close` above — closing a socket's stream closes the
        // socket.
        r.register(os_cls, "close", "()V", ssl_stream_close, cratonvm_native_api::NativeKind::Bridge);
    }

    // NEW-13: SSLSession methods are now backed by the s2_registry TLS id
    // stored at NEW13_SESS_TLSID. Each accessor falls back to the session's
    // own cached field if the id is missing (e.g. the session outlived the
    // TLS stream because the socket was closed), which preserves the ability
    // to call `getProtocol()` / `getCipherSuite()` after close as required
    // by SSLSession javadoc.
    let ssl_session = "javax/net/ssl/SSLSession";
    r.register(
        ssl_session,
        "getProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = if crate::t27_tls::session_shape_width(&*ctx, this) > NEW13_SESS_TLSID {
                crate::t27_tls::sess_get(&*ctx, this, NEW13_SESS_TLSID)
                    .as_int()
                    .unwrap_or(-1)
            } else {
                -1
            };
            if tls_id >= 0 {
                if let Some((p, _, _, _)) = crate::servlet::s2_tls_session_info(tls_id) {
                    let s = ctx.create_string(&p);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            // E12: the cached field is the fallback, but it is not guaranteed
            // to hold a String — `try_alloc_concurrent_synthetic` sizes this
            // object with the REAL layout and the layout guard silently drops
            // writes whose slot type does not match (the same hazard
            // `new13_resolve_socket_session` documents for the session slot),
            // and other modules mint 2-, 4-, 6- and 8-field `SSLSession`s
            // whose slot 0 is not always this one's. Returning the raw slot
            // therefore returned a NULL String from a method JSSE contracts
            // to be non-null. Answer the measured sentinel instead. See
            // `JSSE_NULL_PROTOCOL`.
            match crate::t27_tls::sess_get(&*ctx, this, NEW13_SESS_PROTO) {
                v @ Value::Object(Some(_)) => Ok(Some(v)),
                _ => {
                    let s = ctx.create_string(JSSE_NULL_PROTOCOL);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_session,
        "getCipherSuite",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let tls_id = if crate::t27_tls::session_shape_width(&*ctx, this) > NEW13_SESS_TLSID {
                crate::t27_tls::sess_get(&*ctx, this, NEW13_SESS_TLSID)
                    .as_int()
                    .unwrap_or(-1)
            } else {
                -1
            };
            if tls_id >= 0 {
                if let Some((_, c, _, _)) = crate::servlet::s2_tls_session_info(tls_id) {
                    let s = ctx.create_string(&c);
                    return Ok(Some(Value::Object(Some(s))));
                }
            }
            // E12: same as `getProtocol` just above — a non-String slot must
            // answer the sentinel, not a null String. See
            // `JSSE_NULL_CIPHER_SUITE`.
            match crate::t27_tls::sess_get(&*ctx, this, NEW13_SESS_CIPHER) {
                v @ Value::Object(Some(_)) => Ok(Some(v)),
                _ => {
                    let s = ctx.create_string(JSSE_NULL_CIPHER_SUITE);
                    Ok(Some(Value::Object(Some(s))))
                }
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ssl_session, "isValid", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let tls_id = if crate::t27_tls::session_shape_width(&*ctx, this) > NEW13_SESS_TLSID {
            crate::t27_tls::sess_get(&*ctx, this, NEW13_SESS_TLSID)
                .as_int()
                .unwrap_or(-1)
        } else {
            -1
        };
        // A session is "valid" while the backing TLS stream is still alive
        // in the registry. Closed sockets invalidate their own session.
        let alive = tls_id >= 0 && crate::servlet::s2_tls_session_info(tls_id).is_some();
        Ok(Some(Value::Int(if alive { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_session, "getId", "()[B", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Derive a stable, per-session 32-byte id by hashing the tls_id +
        // negotiated protocol/cipher. Two calls on the same session return
        // the same bytes, per SSLSession.getId() contract.
        let tls_id = if crate::t27_tls::session_shape_width(&*ctx, this) > NEW13_SESS_TLSID {
            crate::t27_tls::sess_get(&*ctx, this, NEW13_SESS_TLSID)
                .as_int()
                .unwrap_or(-1)
        } else {
            -1
        };
        // E12: a session that has negotiated nothing has NO id, and JSSE says
        // so with an EMPTY array rather than with 32 plausible bytes.
        // Measured, HotSpot 25.0.3+9-LTS (`scratchpad/e12/E12SessionContract.java`):
        //
        //     unconnected SSLSocket / pre-handshake SSLEngine  getId() = byte[0]
        //     after a completed TLS 1.3 handshake              getId() = byte[32]
        //
        // The 32 fabricated bytes are the same species of defect as the
        // fabricated cipher name and are arguably worse, because this value's
        // documented use is as an IDENTITY: this registration's own sibling in
        // `t27_tls` says Tomcat reads it "for SSL session tracking". Handing
        // a distinct, stable, non-empty id to a session that never handshaked
        // makes an unnegotiated session look like a trackable one, and
        // `byte[0]` is precisely the signal callers test for.
        //
        // Note the pre-existing bug this also fixes: the seed was derived from
        // `tls_id` alone whenever the registry lookup missed, so EVERY
        // never-connected session shared one id — the opposite of the
        // uniqueness the caller assumes.
        let Some((p, c, _, _)) = crate::servlet::s2_tls_session_info(tls_id) else {
            let empty = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 0);
            return Ok(Some(Value::Object(Some(empty))));
        };
        let mut seed: u64 = (tls_id as i64 as u64).wrapping_mul(0x9E3779B97F4A7C15);
        for b in p.as_bytes().iter().chain(c.as_bytes()) {
            seed = seed.wrapping_mul(1099511628211).wrapping_add(*b as u64);
        }
        let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, 32);
        let mut rng = seed | 1;
        for i in 0..32 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ctx.set_array_element(arr, i, Value::Int(((rng >> 33) & 0xFF) as i32));        }
        Ok(Some(Value::Object(Some(arr))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // NEW-13.4: SSLSession.getPeerCertificates — return a real
    // `java.security.cert.Certificate[]` reconstructed from the peer cert
    // chain DER bytes captured during the native-tls handshake. Throws
    // `SSLPeerUnverifiedException` (surfaced as RuntimeException) if the
    // peer did not present a certificate.
    //
    // F18: this copy LOSES its slot in real-JDK mode to `t27_tls`'s
    // registration (E22-1 §1's `--dump-native-registry` table: `owns_slot =
    // false` here, `true` there), but it is still the twin of the
    // `getPeerPrincipal` below it, and a dead twin that reads a different
    // source is how the live pair drifted apart in the first place. It now
    // calls the same one resolver as its two siblings, so all three see one
    // chain in whichever mode any of them wins — and the width bug described
    // on `getPeerPrincipal` (slot 2 read as a stream id at widths where it is
    // the `isValid` flag) was present here identically and is gone with it.
    r.register(
        ssl_session,
        "getPeerCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = crate::t27_tls::peer_certs_for_session(ctx, this);
            if chain.is_empty() {
                // FIX (tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals):
                // this threw a bare IllegalStateException, contradicting this
                // function's own doc comment above ("Throws
                // SSLPeerUnverifiedException") and the real SSLSession
                // contract -- callers (e.g. Spring's DefaultSslInfo.initCertificates,
                // Apache HttpComponents' AbstractClientTlsStrategy.verifySession)
                // specifically catch SSLPeerUnverifiedException to mean "peer
                // presented no certificate"; an IllegalStateException escaped
                // past that catch instead.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLPeerUnverifiedException",
                    "peer not authenticated",
                ));
            }
            // `X509Certificate[]`, HotSpot's runtime type (i7-L2), as the
            // `t27_tls` twin that owns this slot in real-JDK mode. No raw
            // reference is live here (`chain` is Rust bytes), so it may load.
            let component =
                crate::lang_class::loading_component_id(ctx, "java/security/cert/X509Certificate");
            let arr = crate::util_concurrent_ext::build_rooted_ref_array(
                ctx,
                component,
                chain.len(),
                |ctx, i| {
                    let der = &chain[i];
                    // Allocate a 4-field X509Certificate: the extra field 3
                    // carries the raw DER bytes so `Certificate.getEncoded()`
                    // can return them without relying on
                    // legacy-synthetic-crypto.
                    let cert0 = try_alloc_concurrent_synthetic(
                        ctx,
                        "java/security/cert/X509Certificate",
                        4,
                    )?;
                    // `cert` is live across `create_string`/`new_array` below,
                    // both of which allocate — pin and re-read, same discipline
                    // `build_rooted_ref_array` applies to the array itself.
                    let cert_pin = ctx.pin_native_root(cert0);
                    let (subject, issuer) = basic_der_extract_names(der)
                        .unwrap_or_else(|| ("CN=Unknown".into(), "CN=Unknown".into()));
                    // gc-common w15-d: the subject String is rooted across the
                    // issuer String's allocation (it was stored through its
                    // pre-allocation address).
                    let sub_new = ctx.create_string(&subject);
                    let sub_pin = ctx.pin_native_root(sub_new);
                    let iss_str = ctx.create_string(&issuer);
                    let sub_str = ctx.read_native_pin(sub_pin, sub_new);
                    let cert = ctx.read_native_pin(cert_pin, cert0);
                    ctx.set_field(cert, 0, Value::Object(Some(sub_str)));
                    ctx.set_field(cert, 1, Value::Object(Some(iss_str)));
                    ctx.set_field(cert, 2, Value::Long(0));
                    // Copy DER bytes into a Java byte[] stored at field 3.
                    let der_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, der.len());
                    for (j, &b) in der.iter().enumerate() {
                        ctx.set_array_element(der_arr, j, Value::Int(b as i8 as i32));
                    }
                    let cert = ctx.read_native_pin(cert_pin, cert0);
                    ctx.set_field(cert, 3, Value::Object(Some(der_arr)));
                    ctx.unpin_native_roots(cert_pin);
                    Ok(cert)
                },
            )?;
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // FIX (netty-client-socket-write-after-close residual): getLocalPrincipal/
    // getPeerPrincipal/getLocalCertificates were never registered anywhere in
    // the crate at all — `javax.net.ssl.SSLSession` is a real JDK interface,
    // so a synthetic object impersonating it has no bytecode to fall back to
    // for an unregistered method, and calling one throws AbstractMethodError
    // (observed via Spring's own session-info-building code:
    // `getLocalPrincipal()Ljava/security/Principal; has no Code attribute`).
    r.register(
        ssl_session,
        "getLocalPrincipal",
        "()Ljava/security/Principal;",
        |ctx, args| {
            // STUB-REMOVAL (wave 3): this used to return null unconditionally,
            // which contradicted the sibling `getLocalCertificates` just below —
            // that one was already fixed to read the real chain out of
            // `t27_tls::local_certs_for_session`, because a SERVER session
            // always has its own certificate. `SSLSession.getLocalPrincipal()`
            // is documented as "the principal that was sent to the peer", i.e.
            // the subject of the local leaf certificate, so a session that
            // hands back a chain from `getLocalCertificates()` yet null here
            // was self-contradictory. Derive the principal from that same
            // chain. Null stays correct — and is the documented answer for "no
            // principal was sent" — when there is no local identity (a plain
            // no-mTLS client session), so the original client behaviour is
            // unchanged.
            let this = obj_arg(args, 0)?;
            if crate::t27_tls::in_client_trust_check() {
                return Ok(Some(Value::Object(None)));
            }
            let chain = crate::t27_tls::local_certs_for_session(ctx, this);
            let Some(leaf) = chain.first() else {
                return Ok(Some(Value::Object(None)));
            };
            if real_x500_principal(&*ctx) {
                if let Some(raw) = der_subject_name_raw(leaf) {
                    if let Some(p) = new_real_x500_principal_from_der(ctx, raw) {
                        return Ok(Some(Value::Object(Some(p))));
                    }
                }
            }
            let (subject, _issuer) = basic_der_extract_names(leaf)
                .unwrap_or_else(|| ("CN=Unknown".into(), String::new()));
            // Same 1-field synthetic shape `getPeerPrincipal` builds below.
            // gc-common w15-d: the String first, then the principal (the
            // store went through the principal's pre-allocation address).
            let princ = p68_alloc_with_string_slot(
                ctx,
                "javax/security/auth/x500/X500Principal",
                1,
                0,
                &subject,
            )?;
            Ok(Some(Value::Object(Some(princ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssl_session,
        "getLocalCertificates",
        "()[Ljava/security/cert/Certificate;",
        |ctx, args| {
            // FIX (spring-boot-jetty SecureRequestCustomizer 400 "Invalid SNI"):
            // this used to unconditionally return null, which was only correct
            // for a plain (no-mTLS) CLIENT session. A SERVER session always has
            // a local (its own) certificate chain; Jetty's
            // `SecureRequestCustomizer.getX509()` calls exactly this method on
            // every HTTPS request and throws `HttpException.RuntimeException(400,
            // "Invalid SNI")` when it comes back empty. See
            // `t27_tls::build_synthetic_ssl_session`/`local_certs_for_session`
            // for where the chain is actually populated (client sessions with no
            // configured identity correctly still get an empty chain here).
            let this = obj_arg(args, 0)?;
            // Inside a client-side `checkServerTrusted`, JSSE has not yet sent
            // the client's own certificate — see `in_client_trust_check`.
            if crate::t27_tls::in_client_trust_check() {
                return Ok(Some(Value::Object(None)));
            }
            let chain = crate::t27_tls::local_certs_for_session(ctx, this);
            if chain.is_empty() {
                return Ok(Some(Value::Object(None)));
            }
            // `X509Certificate[]`, HotSpot's runtime type (i7-L2); callers
            // clone and cast it (`(X509Certificate[]) getLocalCertificates()`).
            // No raw reference is live here, so the lookup may load.
            let component =
                crate::lang_class::loading_component_id(ctx, "java/security/cert/X509Certificate");
            let arr = crate::util_concurrent_ext::build_rooted_ref_array(
                ctx,
                component,
                chain.len(),
                |ctx, i| crate::keystore::make_x509_mirror(ctx, "local", &chain[i]),
            )?;
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // F18 — THE DRIFTED TWIN, closed (F10-1 NOMINATION 2, and E31-1
    // NOMINATION 4 / E22-1 NOMINATION B in the same edit).
    //
    // This door and `t27_tls`'s `getPeerCertificates` answer questions about
    // the same fact — what the peer proved about its identity — and they read
    // DIFFERENT SOURCES. `getPeerCertificates` (which owns its slot in
    // real-JDK mode) read the object-keyed `session_peer_certs_table`; this one
    // read `s2_tls_peer_cert_chain_der(slot2)`, the socket registry. An HTTPS
    // client session is in the first and not the second, so on one object, in
    // one call sequence, `getPeerCertificates()` returned the chain while
    // `getPeerPrincipal()` threw `SSLPeerUnverifiedException`.
    //
    // MEASURED on HotSpot 25.0.3+9-LTS (`scratchpad/f18/F18SessionContract.java`,
    // loopback `HttpsServer` + `HttpsURLConnection`, three runs byte-identical):
    //
    //   getPeerPrincipal().getClass()  = javax.security.auth.x500.X500Principal
    //   getPeerPrincipal().getName()   = CN=localhost,OU=F18,O=CratonVM,...
    //   getPeerPrincipal().toString()  = CN=localhost, OU=F18, O=CratonVM, ...
    //   getPeerPrincipal().equals(peerCerts[0].getSubjectX500Principal()) = true
    //
    // That last row is the contract, and it is what makes ONE resolver the
    // right shape rather than merely a tidier one: HotSpot's answer here is
    // *defined* as the subject of the leaf of the chain the sibling returns, so
    // any implementation in which the two can disagree is wrong by
    // construction. `t27_tls::peer_certs_for_session` is now the only function
    // that decides, and both doors call it.
    //
    // The second bug this removes is a cross-connection one and was already on
    // record. The width test was `> NEW13_SESS_TLSID`, i.e. "three or more
    // fields", but slot 2 is a stream id on only some widths — on the 8-field
    // engine session it is the `isValid` FLAG, so this looked up
    // `s2_tls_peer_cert_chain_der(0)` or `(1)`, and `1` is the first id
    // `servlet::s2_next_free_id` ever hands out. A valid engine session could
    // be handed an unrelated socket's peer certificate chain.
    // `session_stream_id` (inside the resolver) is the width table that stops
    // it.
    //
    // The refusal is UNCHANGED and must stay: measured on the same host, a
    // session with no authenticated peer throws
    // `javax.net.ssl.SSLPeerUnverifiedException: peer not authenticated` —
    // exception KIND and message both, and both already correct here.
    r.register(
        ssl_session,
        "getPeerPrincipal",
        "()Ljava/security/Principal;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let chain = crate::t27_tls::peer_certs_for_session(ctx, this);
            let Some(leaf) = chain.first() else {
                // FIX (tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals):
                // same wrong-exception-type bug as getPeerCertificates just
                // above -- see that fix's comment.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "javax/net/ssl/SSLPeerUnverifiedException",
                    "peer not authenticated",
                ));
            };
            if real_x500_principal(&*ctx) {
                if let Some(raw) = der_subject_name_raw(leaf) {
                    if let Some(p) = new_real_x500_principal_from_der(ctx, raw) {
                        return Ok(Some(Value::Object(Some(p))));
                    }
                }
            }
            let (subject, _issuer) = basic_der_extract_names(leaf)
                .unwrap_or_else(|| ("CN=Unknown".into(), String::new()));
            // gc-common w15-d: the String first, then the principal (the
            // store went through the principal's pre-allocation address).
            let princ = p68_alloc_with_string_slot(
                ctx,
                "javax/security/auth/x500/X500Principal",
                1,
                0,
                &subject,
            )?;
            Ok(Some(Value::Object(Some(princ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ssl_session, "getCreationTime", "()J", |_ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Ok(Some(Value::Long(now)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssl_session, "getLastAccessedTime", "()J", |_ctx, _args| {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        Ok(Some(Value::Long(now)))
    }, cratonvm_native_api::NativeKind::Bridge);

    // TrustManagerFactory = 2-field (algorithm=0, keystore=1)
    //
    // The backing X509TrustManager references the same KeyStore reference
    // that was passed to `init(KeyStore)` so that subsequent
    // `getAcceptedIssuers()` / `checkServerTrusted()` calls can walk the
    // stored trust-anchor set. When `init` is called with `null` (the JDK
    // default-store mode) the TrustManager falls back to the VM's system
    // trust roots — mirroring the platform default.
    let tmf = "javax/net/ssl/TrustManagerFactory";
    r.register(
        tmf,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        |ctx, args| {
            // FIX (rabbitautoconfigurationtests-cglib-enhance-hang residual):
            // this used to accept ANY algorithm string unconditionally, so
            // `TrustManagerFactory.getInstance("bogus-algo")` silently
            // succeeded instead of throwing `NoSuchAlgorithmException` like
            // real JDK — breaking `RabbitAutoConfigurationTests.
            // enableSslWithInvalidTrustStoreAlgorithmShouldFail` (and any
            // other caller relying on the JCA `getInstance` contract to
            // reject an unsupported algorithm). Reject up front via the same
            // provider-chain lookup `KeyManagerFactory.getInstance` (below)
            // and `getProvider()` already use, so a caller-registered custom
            // `Provider` service is still honoured.
            let algo_str = match args.get(0) {
                Some(Value::Object(Some(s))) => ctx.read_string(*s).unwrap_or_default(),
                _ => String::new(),
            };
            if crate::jca::provider_chain::find_service_provider_in(
                ctx.vm_identity(),
                "TrustManagerFactory",
                &algo_str,
            )
            .is_none()
            {
                return Err(kmf_tmf_no_such_algorithm(
                    ctx,
                    &format!("{algo_str} TrustManagerFactory not available"),
                ));
            }
            // Field order is the REAL `javax.net.ssl.TrustManagerFactory`
            // declaration order (`javap -p`: provider, factorySpi, algorithm),
            // exactly as the sibling `KeyManagerFactory.getInstance` below
            // already does. The previous layout put the algorithm String at
            // slot 0, so the un-overridden real `getProvider()` bytecode
            // returned a `String` where a `Provider` belongs — the same defect
            // that fix records for KMF, still live here.
            let provider_name = crate::jca::provider_chain::find_service_provider_in(
                ctx.vm_identity(),
                "TrustManagerFactory",
                &algo_str,
            )
            .unwrap_or_else(|| "SunJSSE".to_string());
            let algorithm = args.get(0).copied().unwrap_or(Value::Object(None));
            jsse_factory_alloc(
                ctx,
                "javax/net/ssl/TrustManagerFactory",
                &provider_name,
                algorithm,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        tmf,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("PKIX");
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(tmf, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // A caller's own subclass carries a real `algorithm` its constructor
        // set; reading OUR slot off it returns whatever happens to live there
        // (a `Provider`, for a real receiver). See `jsse_factory_is_ours`.
        if !jsse_factory_is_ours(ctx, this, "javax/net/ssl/TrustManagerFactory") {
            return ctx.invoke_virtual_bytecode_only(
                this,
                "getAlgorithm",
                "()Ljava/lang/String;",
                &[],
            );
        }
        if ctx.object_num_fields(this) > 2 {
            Ok(Some(ctx.get_field(this, 2)))
        } else {
            let s = ctx.create_string("PKIX");
            Ok(Some(Value::Object(Some(s))))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(tmf, "init", "(Ljava/security/KeyStore;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Keyed up front (gc-common w10-e): reading the keystore id and the
        // `javax.net.ssl.trustStore` property below can run Java, which can
        // move `this`; its key is what survives the move. gc-common w27-a:
        // the factory's weak lock key (minted while `this` is current), not
        // its identity hash; its rows -- and the tm id they name -- go with it.
        let this_key = ssl_obj_key(&*ctx, this);
        let this_ih = ctx.identity_hash_code(this);
        let ours = jsse_factory_is_ours(ctx, this, "javax/net/ssl/TrustManagerFactory");
        // gc-common w15-d: the identity key above was not enough -- the
        // receiver and the KeyStore are USED again at the end (the real
        // subclass's own `init`, the initialised mark), after the Java this
        // body runs. Both are pinned; the tail used to take them from `args`.
        let ks_arg = args.get(1).copied().unwrap_or(Value::Object(None));
        let this_pin = ctx.pin_native_root(this);
        let ks_pin = match ks_arg {
            Value::Object(Some(k)) => Some((ctx.pin_native_root(k), k)),
            _ => None,
        };
        // NOTHING is written to a field here. Slot 1 of the real class is
        // `factorySpi`; the write that used to live here destroyed a real
        // subclass's SPI (see `jsse_factory_is_ours` for the measured
        // failure), and for OUR synthetic the keystore is not read back from
        // a field at all — `getTrustManagers()` below resolves it through
        // `tmf_tm_id_by_identity`, which the staging block just after this
        // populates for both receiver kinds.
        // FIX (es-restclient-https): this `TrustManagerFactory.init(KeyStore)`
        // is the one that actually wins (registered last in the lib.rs wiring,
        // shadowing `tls.rs::register_trust_manager_factory`'s otherwise
        // equivalent native — see that function's own matching fix). Without
        // this, an SSLContext built from a caller-supplied truststore (e.g. a
        // test JKS holding a self-signed/test-CA cert — the
        // `HttpsServer`+`RestClient` pattern in
        // `RestClientBuilderIntegTests`) only ever validated peers against the
        // platform root store, so the handshake failed with `UnknownIssuer`
        // even though the JDK caller correctly wired a custom trust store.
        // Stage the keystore's trust anchors on the `t27_tls` per-thread slot
        // so the next real `SSLContext.init` (`net_phase_e.rs`, the winning
        // rustls-backed native) scopes trust to them.
        if let Some(Value::Object(Some(ks))) = args.get(1) {
            let ks_id = crate::tls::read_keystore_registry_id(ctx, *ks);
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).init(KeyStore) this_ih={} ks_id={}",
                    this_ih, ks_id
                );
            }
            if ks_id != 0 {
                let mut state = crate::x509_manager::build_trust_manager_state(ks_id);
                // nio20 wave 4 (L4): which validator JSSE would build
                // (`TrustManagerState::pkix`). Our factory carries the
                // algorithm in slot 2 (`jsse_factory_alloc`); every SunJSSE
                // TMF algorithm but `SunX509` is the PKIX factory.
                // Read as `getAlgorithm` above reads it: no slot means "PKIX".
                if ours {
                    let this_now = ctx.read_native_pin(this_pin, this);
                    state.pkix = if ctx.object_num_fields(this_now) > 2 {
                        match ctx.get_field(this_now, 2) {
                            Value::Object(Some(alg)) => ctx
                                .read_string(alg)
                                .is_some_and(|a| !a.eq_ignore_ascii_case("SunX509")),
                            _ => false,
                        }
                    } else {
                        true
                    };
                }
                if !state.anchor_ders.is_empty() {
                    crate::t27_tls::set_pending_tm_trust_roots(state.anchor_ders.clone());
                }
                // FIX (tomcat-clientauth-engine-config): also register this
                // state into `x509_manager::tm_registry` and stash the
                // resulting id via identity hash, so `getTrustManagers()`
                // below can build a REAL, functional TrustManager (same
                // trap/fix as the sibling
                // `KeyManagerFactory.getKeyManagers()` — see that handler's
                // doc comment for the full story: a bare
                // `try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/
                // X509TrustManager", ...)?` stamps the INTERFACE's own class
                // id, so `checkClientTrusted`/`checkServerTrusted`/
                // `getAcceptedIssuers` have no Code and throw
                // `AbstractMethodError` the moment real bytecode calls one
                // directly instead of going through this crate's own
                // post-handshake `engine_run_trust_check` native path).
                let vm = ctx.vm_identity();
                let tm_id = crate::x509_manager::register_trust_manager_state_in_vm(vm, state);
                let old = tmf_tm_id_by_identity().lock().insert(this_key, tm_id);
                offer_replaced_manager_id(vm, false, old, Some(tm_id));
            }
        } else if ours {
            // A NULL KeyStore is not "no trust store" — it is JSSE's DEFAULT
            // one, and that is the platform roots only when the application
            // has not named its own via `javax.net.ssl.trustStore`. Nothing
            // read the property, so a caller that pinned its trust to one
            // private CA was silently given the whole public root set AND
            // still had its own certificate rejected. See
            // `tls::default_trust_store_keystore_id` for the measurement.
            //
            // Same staging as the non-null branch above, deliberately: once
            // the property names a store, JSSE scopes every default context
            // to it, so the `SSLContext.init` that follows must see it too.
            let explicit = crate::tls::explicit_trust_store_keystore_id(ctx);
            let ks_id = if explicit != 0 {
                explicit
            } else {
                crate::tls::default_trust_store_keystore_id(ctx)
            };
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).init(null) this_ih={} ks_id={} explicit={}",
                    this_ih, ks_id, explicit
                );
            }
            if ks_id != 0 {
                let state = crate::x509_manager::build_trust_manager_state(ks_id);
                // Staged for the next `SSLContext.init` ONLY when the
                // application named the store. Staging `cacerts` would push
                // ~118 anchors into `extra_root_ders`, which the connector
                // adds to the platform set (a union, not JSSE's replace) and
                // which `legacy_dsa_context` then scans for a DSA key — so one
                // DSA root anywhere in the JDK's own trust store could divert
                // unrelated connections onto the legacy OpenSSL path at
                // security level 0. Not a trade worth making for a default.
                if explicit != 0 && !state.anchor_ders.is_empty() {
                    crate::t27_tls::set_pending_tm_trust_roots(state.anchor_ders.clone());
                }
                let vm = ctx.vm_identity();
                let tm_id = crate::x509_manager::register_trust_manager_state_in_vm(vm, state);
                let old = tmf_tm_id_by_identity().lock().insert(this_key, tm_id);
                offer_replaced_manager_id(vm, false, old, Some(tm_id));
            }
        }
        let this_now = ctx.read_native_pin(this_pin, this);
        let ks_now = match ks_pin {
            Some((pin, k)) => Value::Object(Some(ctx.read_native_pin(pin, k))),
            None => ks_arg,
        };
        ctx.unpin_native_roots(this_pin);
        if !ours {
            // A real subclass's `init` means "call MY spi's engineInit" —
            // netty's `SimpleTrustManagerFactory` routes it back to the
            // subclass's own `engineInit(KeyStore)`. Swallowing it left the
            // caller's factory uninitialised while reporting success.
            return ctx.invoke_virtual_bytecode_only(
                this_now,
                "init",
                "(Ljava/security/KeyStore;)V",
                &[ks_now],
            );
        }
        mark_jsse_factory_initialized(ctx, this_now);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        tmf,
        "init",
        "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let ours = jsse_factory_is_ours(ctx, this, "javax/net/ssl/TrustManagerFactory");
            // No field write, for the reason `jsse_factory_is_ours` records:
            // slot 1 is the real class's `factorySpi`.
            // FIX (tomcat-clientauth-engine-config): this overload — used
            // whenever `sslHostConfig.getTruststoreAlgorithm()` is `"PKIX"`
            // (Tomcat's default; see `SSLUtilBase.getTrustManagers()`,
            // which is what the SERVER side of every mTLS test in this
            // suite actually calls) — used to leave `getTrustManagers()`
            // below with no keystore id to work with at all, since
            // `CertPathTrustManagerParameters` carries no back-reference to
            // a `KeyStore`. Reuses `x509_manager`'s own
            // `build_and_register_tm_state_from_mfp` (extracted from that
            // module's `tmf_engine_init_params`, which does the identical
            // walk for its own SPI-delegation registration) to recover the
            // trust anchors directly from `getParameters().
            // getTrustAnchors()` and register a real `TrustManagerState`,
            // then stash the resulting id the same way the `init(KeyStore)`
            // overload above does.
            let mfp = match args.get(1) {
                Some(Value::Object(Some(mfp))) => Some(*mfp),
                _ => None,
            };
            // The anchor walk below is Java (`getParameters()`,
            // `getTrustAnchors()`, `getEncoded()`), so it can move `this`:
            // key it first and pin it across the walk (gc-common w10-e;
            // w27-a: its weak lock key, minted while `this` is current).
            let this_key = ssl_obj_key(&*ctx, this);
            let ih = ctx.identity_hash_code(this);
            let this_pin = ctx.pin_native_root(this);
            // gc-common w15-d: the parameters object too -- a real
            // subclass's own `init` below is handed it AFTER the walk, and
            // it used to be re-read from `args` (its pre-walk address).
            let mfp_pin = mfp.map(|m| (ctx.pin_native_root(m), m));
            let tm_id = crate::x509_manager::build_and_register_tm_state_from_mfp(ctx, mfp);
            let this = ctx.read_native_pin(this_pin, this);
            let mfp_now = match mfp_pin {
                Some((pin, m)) => Value::Object(Some(ctx.read_native_pin(pin, m))),
                None => Value::Object(None),
            };
            ctx.unpin_native_roots(this_pin);
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).init(ManagerFactoryParameters) this_ih={} tm_id={}",
                    ih, tm_id
                );
            }
            let old = tmf_tm_id_by_identity().lock().insert(this_key, tm_id);
            offer_replaced_manager_id(this_key.0, false, old, Some(tm_id));
            if !ours {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "init",
                    "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
                    &[mfp_now],
                );
            }
            mark_jsse_factory_initialized(ctx, this);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        tmf,
        "getTrustManagers",
        "()[Ljavax/net/ssl/TrustManager;",
        |ctx, args| {
            // FIX (tomcat-clientauth-engine-config): this used to always
            // return a bare `try_alloc_concurrent_synthetic(ctx,
            // "javax/net/ssl/X509TrustManager", 2)?` — an object stamped
            // with the INTERFACE's own class id. `getAcceptedIssuers()` on
            // such an object was already known to throw
            // `AbstractMethodError` (see the `FIX
            // (x509-trustmanager-abstractmethod-20260707)` narrow rescue
            // wired in `lib.rs` via `t27_tls::register_accepted_issuers`,
            // registered directly on the bare interface since accepted-
            // issuers enumeration is instance-independent — platform trust
            // roots). `checkClientTrusted`/`checkServerTrusted` need real
            // PER-INSTANCE state (which keystore's anchors to validate
            // against), which a single interface-wide native cannot
            // provide, so they were never given the same rescue and kept
            // throwing `AbstractMethodError` for any caller invoking one
            // directly (confirmed via this session's `CRATONVM_DBG_NOCODE`
            // tracing against Tomcat's `TestClientCert`/
            // `engine_run_trust_check`'s post-handshake
            // `checkClientTrusted` call — see
            // `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`, "Residual #2 implementation" for the full
            // trace). Fixed the same way as the sibling
            // `KeyManagerFactory.getKeyManagers()` fix just above: build
            // the real, functional `FQN_X509_TM`-shaped object
            // `x509_manager.rs`'s `tmf_engine_get_trust_managers` already
            // produces for the SPI-delegation path, keyed off the SAME
            // `tm_registry` id either `init` overload above (`KeyStore` or
            // `ManagerFactoryParameters`) already registered and stashed
            // via `tmf_tm_id_by_identity`. The default/null-initialized path
            // uses id 0 on the same real-impl-shaped object, which the
            // `x509_manager` handlers intentionally resolve as platform
            // default trust roots. This avoids falling back to the old (non-
            // functional but allocation-safe) stub when no id was captured
            // (matches original behavior for `init((KeyStore) null)` / a
            // `getTrustManagers()` call with no preceding `init`).
            let this = obj_arg(args, 0)?;
            let runtime_class = ctx
                .class_name_of_id(ctx.class_id_of_object(this))
                .unwrap_or_default();
            // This bridge owns only the synthetic default factory made by
            // TrustManagerFactory.getInstance(). A concrete provider factory
            // (notably Netty's InsecureTrustManagerFactory) implements its
            // policy through the real TrustManagerFactory bytecode and SPI.
            // Treating every subclass as our synthetic default silently
            // replaces that provider's manager with a PKIX manager. Execute
            // the base bytecode without re-entering this native so virtual
            // SPI dispatch returns the provider's configured manager.
            if runtime_class != "javax/net/ssl/TrustManagerFactory" {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "getTrustManagers",
                    "()[Ljavax/net/ssl/TrustManager;",
                    &[],
                );
            }
            require_jsse_factory_initialized(ctx, this, "TrustManagerFactoryImpl")?;
            let ih = ctx.identity_hash_code(this);
            // gc-common w27-a: by the factory's own lock key (an `init`ed
            // factory has one), never another live factory's row.
            let tm_id = match existing_ssl_obj_key(ctx, this) {
                Some(key) => tmf_tm_id_by_identity()
                    .lock()
                    .get(&key)
                    .copied()
                    .unwrap_or(0),
                None => 0,
            };
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!(
                    "[dbg-tls-auth] tmf(phases_late).getTrustManagers this_ih={} looked_up_tm_id={}",
                    ih, tm_id
                );
            }
            let tm = try_alloc_concurrent_synthetic(ctx, crate::x509_manager::FQN_X509_TM, 2)?;
            crate::x509_manager::set_tm_id(ctx, tm, tm_id);
            // `TrustManager[]`, the declared type (i6-L2). The array allocation can
            // move `tm` (gc-common w10-e).
            let component =
                crate::lang_class::reflection_component_id(ctx, "javax/net/ssl/TrustManager");
            let tm_pin = ctx.pin_native_root(tm);
            let arr = ctx.new_ref_array(component, 1);
            let tm = ctx.read_native_pin(tm_pin, tm);
            ctx.unpin_native_roots(tm_pin);
            ctx.set_array_element(arr, 0, Value::Object(Some(tm)));
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // KeyManagerFactory = 3-field, matching the REAL javax.net.ssl.KeyManagerFactory
    // field declaration order exactly (`javap -p javax.net.ssl.KeyManagerFactory`:
    // `provider`, `factorySpi`, `algorithm`, in that order — NOT the order fields
    // are first referenced in the constant pool, which is a different, easy-to-
    // misread ordering). `getProvider()`/`getAlgorithm()` have no native override
    // here, so they fall through to real bytecode reading these exact slots
    // (`getProvider()` returns field 0, `getAlgorithm()` returns field 2 — see
    // `javap -c`). The previous field numbering here (algorithm=0, keystore=1,
    // password=2) put the algorithm *String* at slot 0, so real bytecode's
    // `getProvider()` returned that String in place of a `Provider` — callers
    // invoking `.getInfo()` on it (e.g. `SSLUtilBase.getKeyManagers`) got
    // `NoSuchMethodError: java/lang/String.getInfo()`.
    let kmf = "javax/net/ssl/KeyManagerFactory";
    r.register(
        kmf,
        "getInstance",
        "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        |ctx, args| {
            // gc-common w15-d: the factory is allocated LAST, by
            // `jsse_factory_alloc` (it used to be allocated here, before the
            // provider object below -- whose construction allocates and can
            // run Java -- and was then written through its old address).
            //
            // field 0: provider — a real `Provider` object, not the SPI's
            // factorySpi/algorithm, so `getProvider().getInfo()` et al. work.
            // Built via `provider_chain`'s own `make_provider` (not a raw
            // `alloc_concurrent_synthetic` + indexed `set_field`): in
            // real-JDK mode `alloc_concurrent_synthetic` upsizes the object
            // to `java/security/Provider`'s full real field count —
            // inherited Hashtable/Properties fields included — so a plain
            // slot-0/1 write lands on whatever field happens to occupy that
            // slot in the real inheritance layout, not `name`/`version`.
            // `make_provider` writes by field NAME (`set_field_by_name`)
            // specifically to survive that, and its registered `getInfo()`
            // native reads the same "info" field back by name — see
            // `jca/provider_chain.rs`'s module doc for the full story.
            //
            // FIX (keymanagerfactoryfips-bare-assertion): this used to
            // hardcode "SunJSSE" unconditionally, so ANY caller who
            // registered their own `KeyManagerFactory.<algo>` service on a
            // custom `Provider` (via `Security.addProvider` +
            // `Provider.put`/`putService`) got the wrong provider back —
            // `getProvider()` always pointed at the built-in SunJSSE
            // placeholder instead of the caller's own `Provider` instance.
            // Tomcat's `TestKeyManagerWrappingFips.testBug64614_01` hits
            // this directly: it registers a dummy `KeyManagerFactory`
            // service on a provider whose `getInfo()` contains "FIPS", and
            // `SSLUtilBase.getKeyManagers()` branches on
            // `kmf.getProvider().getInfo().contains("FIPS")` — which always
            // read the SunJSSE placeholder's info (no "FIPS") instead.
            // `find_service_provider` searches the real provider chain
            // (which already includes providers added via
            // `Security.addProvider`) for who registered this algorithm,
            // falling back to "SunJSSE" — the previous hardcoded default —
            // when nobody has, so the existing SunX509/NewSunX509/PKIX
            // callers are unaffected.
            let algorithm = args.get(0).copied().unwrap_or(Value::Object(None));
            // MEASURED on HotSpot 25.0.4+7:
            // `KeyManagerFactory.getInstance(null)` is
            // `NullPointerException: null algorithm name`
            // (`Objects.requireNonNull(algorithm, "null algorithm name")`),
            // NOT a `NoSuchAlgorithmException`. The empty-string fallback
            // below turned it into ` KeyManagerFactory not available` — the
            // wrong TYPE, so a `catch (NoSuchAlgorithmException)` swallowed a
            // programming error the JDK reports as one.
            if matches!(algorithm, Value::Object(None)) {
                return Err(cratonvm_types::error::RuntimeError::NullPointerException {
                    message: Some("null algorithm name".to_string()),
                }
                .into());
            }
            let algo_str = match algorithm {
                Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                _ => String::new(),
            };
            // FIX (rabbitautoconfigurationtests-cglib-enhance-hang residual):
            // no provider (built-in or caller-registered) claims this
            // algorithm — real JDK's `getInstance` throws
            // `NoSuchAlgorithmException` here rather than silently falling
            // back to a default implementation. Was previously unconditional
            // (`unwrap_or_else(|| "SunJSSE")`), so `KeyManagerFactory.
            // getInstance("bogus-algo")` always succeeded — breaking
            // `RabbitAutoConfigurationTests.
            // enableSslWithInvalidKeyStoreAlgorithmShouldFail` (and any other
            // caller relying on the JCA contract to reject an unsupported
            // algorithm).
            let found_provider = crate::jca::provider_chain::find_service_provider_in(
                ctx.vm_identity(),
                "KeyManagerFactory",
                &algo_str,
            );
            if found_provider.is_none() {
                return ctx.invoke_special_bytecode_only(
                    "javax/net/ssl/KeyManagerFactory",
                    "getInstance",
                    "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
                    args,
                );
            }
            let provider_name = found_provider.unwrap_or_else(|| "SunJSSE".to_string());
            jsse_factory_alloc(
                ctx,
                "javax/net/ssl/KeyManagerFactory",
                &provider_name,
                algorithm,
            )
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        kmf,
        "getDefaultAlgorithm",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let s = ctx.create_string("SunX509");
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(kmf, "getAlgorithm", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if !jsse_factory_is_ours(ctx, this, "javax/net/ssl/KeyManagerFactory") {
            return ctx.invoke_virtual_bytecode_only(
                this,
                "getAlgorithm",
                "()Ljava/lang/String;",
                &[],
            );
        }
        if ctx.object_num_fields(this) > 2 {
            Ok(Some(ctx.get_field(this, 2)))
        } else {
            let s = ctx.create_string("SunX509");
            Ok(Some(Value::Object(Some(s))))
        }
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(kmf, "init", "(Ljava/security/KeyStore;[C)V", |ctx, args| {
        // Fields 0-2 (provider/factorySpi/algorithm) are fixed at construction
        // to match the real class layout — init() doesn't touch them. The
        // thread-local identity staging below (per-SSLContext mTLS identity
        // flow) is one consumer of the keystore this KMF was init'd with.
        //
        // FIX (tomcat-clientauth-engine-config): ALSO stash the keystore's
        // registry id directly on this KMF object (via `set_field_by_name`'s
        // pseudo-field convention — a real `javax.net.ssl.KeyManagerFactory`
        // has no room for it, same reasoning as `x509_manager.rs`'s
        // `cratonvm$x509km$id`/`cratonvm$x509tm$id` side-fields), so
        // `getKeyManagers()` below can build a REAL, functional KeyManager
        // instead of the non-functional bare-interface stub it used to
        // return (see that handler's doc comment for the full story).
        //
        // gc-common w10-e: the receiver's identity key is taken HERE, before
        // the password read and the keystore walk (both can run Java and move
        // the receiver, whose `args` copy is not updated), and carries the
        // calling VM, since identity hashes collide across VMs. gc-common
        // w27-a: the factory's weak lock key, so a live factory that shares
        // this one's hash keeps its own rows; they go with the key.
        let this_key = obj_arg(args, 0).ok().map(|t| ssl_obj_key(&*ctx, t));
        let this_ih = obj_arg(args, 0)
            .ok()
            .map(|t| ctx.identity_hash_code(t))
            .unwrap_or(0);
        // A (re-)`init` invalidates the key manager a previous
        // `getKeyManagers()` built (gc-common w11-a, see
        // `kmf_key_manager_cache`).
        if let Some(key) = this_key {
            let old = kmf_key_manager_cache().lock().remove(&key);
            // gc-common w13-a: the state the old row named is offered (its
            // `KeyManager`s may still carry it).
            offer_replaced_manager_id(key.0, true, old.map(|(_, km)| km), None);
            // gc-common w14-f handoff: a re-`init` also ends what the
            // previous one recorded in the other two rows; the branches below
            // write the row this `init` needs. Otherwise `getKeyManagers()`
            // keeps serving the first `init`'s keystore (and keeps its store
            // alive until the factory dies).
            kmf_keystore_id_by_identity().lock().remove(&key);
            let old_live = kmf_live_km_id_by_identity().lock().remove(&key);
            offer_replaced_manager_id(key.0, true, old_live, None);
        }
        // gc-common w15-d: the password read, the pending-identity staging
        // and the live-keystore walk below all run Java. The KeyStore and the
        // password array were used after them through the addresses in
        // `args`, and so were the receiver (the real subclass's own `init`,
        // the initialised mark) and both arguments handed on to it. All three
        // are rooted here and re-read at every use after a GC point.
        let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
        let arg_handles: Vec<Option<cratonvm_native_api::NativeHandle>> = args
            .iter()
            .take(3)
            .map(|v| match v {
                Value::Object(Some(o)) => Some(scope.root(*o)),
                _ => None,
            })
            .collect();
        let fresh_arg = |scope: &cratonvm_native_api::NativeHandleScope, i: usize| -> Value {
            match (arg_handles.get(i), args.get(i)) {
                (Some(Some(h)), _) => Value::Object(Some(scope.get(h))),
                (_, Some(v)) => *v,
                _ => Value::Object(None),
            }
        };
        let fresh_obj =
            |scope: &cratonvm_native_api::NativeHandleScope, i: usize| -> Option<ObjectRef> {
                match fresh_arg(scope, i) {
                    Value::Object(Some(o)) => Some(o),
                    _ => None,
                }
            };
        if let Some(ks_first) = fresh_obj(&scope, 1) {
            let password_arg = fresh_arg(&scope, 2);
            let key_password = if args.len() > 2 {
                crate::keystore::read_password(&mut *scope, &password_arg)
            } else {
                Default::default()
            };
            if crate::nbflags().dbg_tls_auth {
                let ks_dbg = fresh_obj(&scope, 1).unwrap_or(ks_first);
                eprintln!(
                    "[dbg-tls-auth] kmf(phases_late).init(KeyStore) this_ih={} ks_id={} password_len={}",
                    this_ih,
                    crate::keystore::keystore_id_from_object(&mut *scope, ks_dbg),
                    key_password.len()
                );
            }
            let ks = fresh_obj(&scope, 1).unwrap_or(ks_first);
            crate::keystore::keystore_set_pending_km_identity_with_password(
                &mut *scope,
                ks,
                &key_password,
            );
            let ks = fresh_obj(&scope, 1).unwrap_or(ks_first);
            let ks_id = crate::keystore::keystore_id_from_object(&mut *scope, ks);
            if ks_id == 0 {
                // A `KeyStore` this VM has no native record of — an application's
                // own `KeyStore` subclass over its own `KeyStoreSpi`. netty's
                // `OpenSslX509KeyManagerFactory.newKeyless` is exactly that, and
                // it reaches THIS shim because netty's own factory SPI builds a
                // default `KeyManagerFactory` and inits it with that store.
                // Enumerating the store through its own bytecode is the only way
                // to serve it; see
                // `x509_manager::build_key_manager_state_from_live_keystore` for
                // what the previous bare-interface fallback cost.
                obj_arg(args, 0)?;
                if let Some(key) = this_key {
                    let ks = fresh_obj(&scope, 1).unwrap_or(ks_first);
                    let pw_obj = fresh_obj(&scope, 2);
                    let mut state = crate::x509_manager::build_key_manager_state_from_live_keystore(
                        &mut *scope,
                        ks,
                        pw_obj,
                    );
                    state.vm = scope.vm_identity();
                    if crate::nbflags().dbg_tls_auth {
                        eprintln!(
                            "[dbg-tls-auth] kmf(phases_late).init(live KeyStore) this_ih={} aliases={}",
                            this_ih,
                            state.aliases_to_chain.len()
                        );
                    }
                    if !state.aliases_to_chain.is_empty() {
                        let km_id = crate::x509_manager::next_km_id();
                        crate::x509_manager::km_registry().write().insert(km_id, state);
                        let old = kmf_live_km_id_by_identity().lock().insert(key, km_id);
                        offer_replaced_manager_id(key.0, true, old, Some(km_id));
                    }
                }
            }
            if ks_id != 0 {
                obj_arg(args, 0)?;
                // `set_field_by_name` would silently no-op here: this KMF
                // object is allocated with the REAL `javax/net/ssl/
                // KeyManagerFactory` class shape (fields 0-2 already fixed to
                // provider/factorySpi/algorithm above), which declares no
                // `cratonvm$...` pseudo-field for a real class to land on —
                // same trap this doc's helpers (`keystore.rs::
                // store_id_by_identity`, `x509_manager.rs::km_id_by_identity`/
                // `tm_id_by_identity`) were all built to route around. Use an
                // identity-hash side table directly rather than
                // `set_field_by_name`/`set_field`, neither of which has
                // anywhere to durably land the value on this object shape.
                if let Some(key) = this_key {
                    kmf_keystore_id_by_identity().lock().insert(key, ks_id);
                }
            }
        }
        let this_now: ObjectRef = match fresh_obj(&scope, 0) {
            Some(t) => t,
            None => obj_arg(args, 0)?,
        };
        if !jsse_factory_is_ours(&mut *scope, this_now, "javax/net/ssl/KeyManagerFactory") {
            let ks_now = fresh_arg(&scope, 1);
            let pw_now = fresh_arg(&scope, 2);
            return scope.invoke_virtual_bytecode_only(
                this_now,
                "init",
                "(Ljava/security/KeyStore;[C)V",
                &[ks_now, pw_now],
            );
        }
        mark_jsse_factory_initialized(&*scope, this_now);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    // STUB-REMOVAL (wave 2): this used to return normally without doing
    // anything, which is the worst possible answer — the caller believes its
    // key material was accepted, `getKeyManagers()` then hands back a manager
    // with no identity, and the client certificate is silently never presented.
    // The real `SunX509` / `NewSunX509` `KeyManagerFactorySpi.engineInit
    // (ManagerFactoryParameters)` throws `InvalidAlgorithmParameterException`
    // for every parameter type it does not understand (SunX509 understands
    // none; NewSunX509 understands only `KeyStoreBuilderParameters`), and this
    // implementation understands none either — the KeyStore overload above is
    // the only supported initialisation path. Fail honestly so the caller sees
    // the same exception the reference JDK raises instead of a mute downgrade.
    r.register(
        kmf,
        "init",
        "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
        |ctx, args| {
            // Only the synthetic default factory is unable to serve this
            // overload. A caller's own subclass (netty's
            // `SimpleKeyManagerFactory`, and every per-test factory built on
            // it) has a real SPI that implements it — refusing on its behalf
            // turned a working provider into a hard failure.
            if let Ok(this) = obj_arg(args, 0) {
                if !jsse_factory_is_ours(ctx, this, "javax/net/ssl/KeyManagerFactory") {
                    return ctx.invoke_virtual_bytecode_only(
                        this,
                        "init",
                        "(Ljavax/net/ssl/ManagerFactoryParameters;)V",
                        &[args.get(1).copied().unwrap_or(Value::Object(None))],
                    );
                }
            }
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/security/InvalidAlgorithmParameterException",
                "KeyManagerFactory.init(ManagerFactoryParameters) is not supported; \
                 use init(KeyStore, char[])",
            ))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        kmf,
        "getKeyManagers",
        "()[Ljavax/net/ssl/KeyManager;",
        |ctx, args| {
            // FIX (tomcat-clientauth-engine-config): this handler used to
            // return a bare `try_alloc_concurrent_synthetic(ctx,
            // "javax/net/ssl/X509KeyManager", 0)?` — an object stamped with
            // the INTERFACE's own class id (it's a real, loadable JDK
            // interface, so `ensure_class_initialized` happily "succeeds").
            // Every one of `X509KeyManager`'s 6 methods is abstract with no
            // Code, so real Java bytecode calling any of them directly
            // (rather than going through this crate's own
            // `t27_tls::JavaKeyManagerResolver`, which never calls
            // `getKeyManagers()` at all — it captures the KeyManager array
            // straight off `SSLContext.init()`'s arguments instead) hit an
            // unconditional `AbstractMethodError`. Confirmed via direct
            // `CRATONVM_DBG_NOCODE=1` tracing against Tomcat's
            // `TesterSupport.TrackingKeyManager` (`test/org/apache/tomcat/
            // util/net/TesterSupport.java`), whose `chooseClientAlias`
            // override does exactly this
            // (`manager.chooseClientAlias(keyType, issuers, socket)`,
            // `manager` being whatever `getKeyManagers()` returned) — see
            // `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`, "Residual #2 implementation" for the full trace.
            //
            // Fixed by building the SAME real, natively-backed
            // `FQN_SUN_X509_KM`-shaped object `x509_manager.rs`'s
            // `kmf_engine_get_key_managers` already produces for the SPI
            // delegation path (`KeyManagerFactoryImpl$SunX509.
            // engineGetKeyManagers`, reached when a caller goes through the
            // real `factorySpi` chain) — `chooseClientAlias`/
            // `chooseServerAlias`/`getCertificateChain`/`getClientAliases`/
            // `getPrivateKey`/`getServerAliases` are all real, working
            // natives on that class (`x509_manager::register_key_manager`),
            // so a wrapper's direct delegate call now completes instead of
            // throwing. Registered in the SAME `km_registry` +
            // `next_km_id`/`set_km_id` id space so `choose_client_alias`/
            // `get_private_key`'s registry lookups resolve correctly. Falls
            // back to the old (non-functional but at least allocation-safe)
            // stub only when no keystore id was ever captured — matches
            // this handler's original behavior for a `getKeyManagers()`
            // call with no preceding `init(KeyStore, char[])`, which is not
            // a real/expected call shape for this API but shouldn't panic.
            let this = obj_arg(args, 0)?;
            // Sibling of the `TrustManagerFactory.getTrustManagers` guard just
            // above: a concrete provider factory (netty's
            // `SimpleKeyManagerFactory`, e.g. `SniClientJava8TestUtil`'s
            // per-host key manager) implements its policy through the real
            // bytecode and its own SPI. Answering with OUR keystore-derived
            // manager silently replaces the caller's.
            if !jsse_factory_is_ours(ctx, this, "javax/net/ssl/KeyManagerFactory") {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "getKeyManagers",
                    "()[Ljavax/net/ssl/KeyManager;",
                    &[],
                );
            }
            require_jsse_factory_initialized(ctx, this, "KeyManagerFactoryImpl")?;
            let vm = ctx.vm_identity();
            // gc-common w27-a: the factory's own lock key. An `init`ed factory
            // (the gate above) already has one, so this does not mint; it is
            // taken here, before the allocations below, because the cache row
            // is written after `build_key_manager_state`.
            let key = ssl_obj_key(&*ctx, this);
            let ks_id = kmf_keystore_id_by_identity()
                .lock()
                .get(&key)
                .copied()
                .unwrap_or(0);
            // Slot 2 is the algorithm this factory was created with (see
            // `KeyManagerFactory.getInstance` above). It decides WHICH
            // KeyManager class the JDK hands out, and callers branch on
            // that name — see `km_mirror_class_for_algorithm`. Read once, up
            // front, for the two branches that use it (it was read inside
            // each; one read of `this` before any allocation is simpler to
            // check).
            let algorithm = if ctx.object_num_fields(this) > 2 {
                match ctx.get_field(this, 2) {
                    Value::Object(Some(s)) => ctx.read_string(s).unwrap_or_default(),
                    _ => String::new(),
                }
            } else {
                String::new()
            };
            let km = if ks_id != 0 {
                let mirror = crate::x509_manager::km_mirror_class_for_algorithm(&algorithm);
                // One state per `init`, not per call (gc-common w11-a): reuse
                // the id an earlier call built from this same keystore, as long
                // as its row is still there.
                let cached = {
                    let row = kmf_key_manager_cache().lock().get(&key).copied();
                    row
                }
                .filter(|&(cached_ks, _)| cached_ks == ks_id)
                .map(|(_, km_id)| km_id)
                .filter(|km_id| {
                    crate::x509_manager::km_registry()
                        .read()
                        .contains_key(km_id)
                });
                let km_id = match cached {
                    Some(km_id) => km_id,
                    None => {
                        let mut state = crate::x509_manager::build_key_manager_state(ks_id);
                        // Stamped so the row goes with this VM's teardown (w10-e).
                        state.vm = vm;
                        let km_id = crate::x509_manager::next_km_id();
                        crate::x509_manager::km_registry()
                            .write()
                            .insert(km_id, state);
                        let old = kmf_key_manager_cache().lock().insert(key, (ks_id, km_id));
                        offer_replaced_manager_id(vm, true, old.map(|(_, km)| km), Some(km_id));
                        km_id
                    }
                };
                let km = try_alloc_concurrent_synthetic(ctx, mirror, 2)?;
                crate::x509_manager::set_km_id(ctx, km, km_id);
                km
            } else if let Some(km_id) = {
                // Bound in a block so the guard drops before the arm's body,
                // which reads fields through `ctx` — a re-entry into the VM
                // this table's `LockLevel` promises never happens under it.
                let live = kmf_live_km_id_by_identity().lock().get(&key).copied();
                live
            } {
                // A caller's own `KeyStore`, already enumerated at `init` time.
                // Same mirror class and the same `km_registry` id space as the
                // branch above, so `getCertificateChain`/`getPrivateKey` are the
                // real natives rather than abstract interface methods.
                let mirror = crate::x509_manager::km_mirror_class_for_algorithm(&algorithm);
                let km = try_alloc_concurrent_synthetic(ctx, mirror, 2)?;
                crate::x509_manager::set_km_id(ctx, km, km_id);
                km
            } else {
                // Nothing was ever `init`-ed with a usable store. Still answer
                // with the natively-backed mirror rather than a bare-interface
                // object: an EMPTY registry entry makes `getCertificateChain`
                // return null, which is a contract-legal answer, where
                // `AbstractMethodError` is not.
                let km_id = crate::x509_manager::next_km_id();
                crate::x509_manager::km_registry().write().insert(
                    km_id,
                    crate::x509_manager::KeyManagerState {
                        vm,
                        ..Default::default()
                    },
                );
                let km = try_alloc_concurrent_synthetic(
                    ctx,
                    crate::x509_manager::km_mirror_class_for_algorithm(""),
                    2,
                )?;
                crate::x509_manager::set_km_id(ctx, km, km_id);
                km
            };
            // `KeyManager[]`, the declared type (i6-L2). The array allocation can
            // move `km` (gc-common w10-e).
            let component =
                crate::lang_class::reflection_component_id(ctx, "javax/net/ssl/KeyManager");
            let km_pin = ctx.pin_native_root(km);
            let arr = ctx.new_ref_array(component, 1);
            let km = ctx.read_native_pin(km_pin, km);
            ctx.unpin_native_roots(km_pin);
            ctx.set_array_element(arr, 0, Value::Object(Some(km)));
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // SSLEngine = 7-field (client_mode=0, need_client_auth=1, want_client_auth=2,
    //                       enabled_protocols=3, enabled_cipher_suites=4,
    //                       handshake_status=5, session=6)
    //
    // ─── WHO OWNS THIS TRIPLE SET, MEASURED 2026-08-12 (W7-61) ───────────────
    //
    // The real `javax.net.ssl.SSLEngine` declares TWO instance fields on JDK
    // 25.0.3.9 — `private String peerHost` (slot 0) and `private int peerPort`
    // (slot 1) — so this 7-slot map, applied to a real-layout instance, would
    // write an `Int` into a reference field and five values past the end. That
    // is the species `docs/architecture/natives-over-real-jdk-classes.md` §5
    // calls heap corruption, and W7-49-slot-index-recensus.md lists it as LIVE.
    //
    // It is NOT live, and the reason is last-write-wins. Do not "repair" this
    // map: a renumber here is inert in Compatible mode and would break the only
    // mode where it IS the winner. The ordering, re-derived by a brace-depth
    // scan of the registrar bodies rather than from indentation (indentation
    // lies in this file — several nested `fn`s inside `register_p68_ssl` close
    // at column 0, which makes a column-0 scan put line 4575 outside it):
    //
    //   Compatible / strict, `register_essential_natives_with_shims`
    //   (lib.rs 6878..20732), every call below at brace depth 1, i.e.
    //   unconditional, in source order:
    //     18191  register_p68_ssl              — this map, plus
    //                                            SSLContext.createSSLEngine x2
    //                                            -> `ssleng_alloc` (requests 7)
    //     18214  net_phase_e::register_phase_e_networking
    //              -> register_re6_ssl_context — RE-REGISTERS both
    //                                            createSSLEngine descriptors,
    //                                            allocating
    //                                            `sun/security/ssl/SSLEngineImpl`
    //     18252  t27_tls::register_sslengine_real
    //                                          — 32 triples on
    //                                            `sun/security/ssl/SSLEngineImpl`,
    //                                            keyed by a side table, a
    //                                            superset by (name, descriptor)
    //                                            of the 21 registered here
    //   => `ssleng_alloc` is DEAD in Compatible mode: both of its entry points
    //      are overwritten 23 lines later, and lib.rs's own comment at 18192
    //      says that ordering is deliberate and load-bearing.
    //   => the 21 triples below survive registration but have no receiver.
    //      `invoke.rs`'s `invoke_class` for a non-special, non-array virtual
    //      call is the RECEIVER's runtime class name (invoke.rs:1051, the final
    //      `else` arm reads `args[0]`'s class_id), so step 1 looks this map up
    //      under the receiver's own class; and the hierarchy walk that would
    //      otherwise reach an abstract superclass is skipped for
    //      `walk_native_hierarchy == false` whenever the receiver's class
    //      declares the method itself (invoke.rs:3271). `javax/net/ssl/SSLEngine`
    //      is abstract, so the only receivers that could land here are ones
    //      CratonVM allocated on the abstract class itself — and in Compatible
    //      mode nothing does.
    //
    //   Synthetic (`use_synthetic_jdk == true`, vm_init.rs:1580
    //   `register_builtins` = essential then `register_synthetic_overrides`):
    //     23713  register_tls_natives          — tls.rs, `alloc_ssl_engine`
    //                                            (requests 14) + 19 triples on
    //                                            a 14-slot map
    //     23716  register_phase68_natives      — reaches `register_p68_ssl`
    //                                            AGAIN, so THIS map and
    //                                            `ssleng_alloc` win back
    //   => p68 is the last writer in BOTH modes. lib.rs:23708 states that
    //      ordering explicitly ("Registered BEFORE phase68 so that
    //      register_p68_ssl's ... implementations take precedence").
    //
    // What IS wrong, and is left named rather than half-repaired: in synthetic
    // mode the 8 tls.rs triples this function does not re-register keep the
    // 14-slot map on a 7-wide object — `getApplicationProtocol` (slot 7),
    // `setSSLParameters`/`getSSLParameters` (slots 8-13) and `<init>` (writes
    // 0-13) all address past the end, and `getPeerHost`/`getPeerPort` read
    // slots 5/6, which this map uses for handshake_status and session. Two maps
    // on one class is the shape that made `java.lang.Process` a bug. Repairing
    // it means giving the surface ONE owner, which moves both this file and
    // `tls.rs` in one step and re-bases the `vm/src/vm/tests.rs` fixture that
    // pins this map — a change that needs a build, and is not this lane's.
    // `registry_ordering_tests::p68_ssl_is_the_last_writer_on_the_ssl_engine_surface`
    // in `tls.rs` is the ratchet that makes a silent reordering fail.
    let ssleng = "javax/net/ssl/SSLEngine";
    // Slot 0 of the real `sun.security.ssl.SSLEngineImpl` this VM allocates is
    // `javax.net.ssl.SSLEngine.peerHost`, a REFERENCE. The `Int` written here
    // was inert and the read handed a String slot back as a boolean, which is
    // why a fresh engine reported `getUseClientMode() == true` where HotSpot
    // reports `false` (`L6TlsParamSweep` rows 66 and 89). The role belongs
    // with the rest of the engine's state.
    r.register(ssleng, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        crate::t27_tls::set_engine_use_client_mode(ctx, this, v != 0);
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(i32::from(
            crate::t27_tls::engine_use_client_mode(ctx, this),
        ))))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 1, Value::Int(v));
        // setNeedClientAuth(true) clears wantClientAuth and vice versa
        if v != 0 {
            ctx.set_field(this, 2, Value::Int(0));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
        ctx.set_field(this, 2, Value::Int(v));
        if v != 0 {
            ctx.set_field(this, 1, Value::Int(0));        }
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, 2)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ssleng,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 3, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssleng,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, 3) {
                Value::Object(Some(arr)) => Ok(Some(Value::Object(Some(arr)))),
                // E42: the pair was spelled `["TLSv1.2", "TLSv1.3"]` under the
                // comment `// Default: TLSv1.2, TLSv1.3`. Measured on HotSpot
                // (`scratchpad/e42/E42EnabledSets.java`) a fresh engine answers
                // `[TLSv1.3, TLSv1.2]` — most-preferred first, and the order is
                // observable. See `JSSE_ENABLED_PROTOCOLS`.
                _ => Ok(Some(Value::Object(Some(jsse_enabled_protocol_array(ctx)?)))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssleng,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            ctx.set_field(this, 4, args.get(1).copied().unwrap_or(Value::Object(None)));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssleng,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            match ctx.get_field(this, 4) {
                Value::Object(Some(arr)) => Ok(Some(Value::Object(Some(arr)))),
                // E42 — this was a ONE-element list holding
                // `TLS_AES_128_GCM_SHA256`, and the comment on
                // `getSupportedCipherSuites` below justified the gap between
                // the two accessors as "what's already modeled by the
                // getEnabledCipherSuites default branch above". Nothing was
                // modelled: the branch was a single hard-coded suite name.
                //
                // Measured, HotSpot 25.0.3+9-LTS
                // (`scratchpad/e42/E42EnabledSets.java`), on an engine from
                // `SSLContext.getInstance("TLS").createSSLEngine()`:
                //
                //     enabledCipherSuites.count   = 31
                //     supportedCipherSuites.count = 31
                //     enabled equals supported    = true
                //
                // There is no enabled/supported distinction to model, and the
                // SOCKET door in this same file already knew that — its
                // `getEnabledCipherSuites` and `getSupportedCipherSuites` both
                // answer `SUPPORTED_CIPHER_SUITE_NAMES`. So the two doors of
                // one VM disagreed: one offered 15 suites, the other 1. Netty's
                // `JdkSslContext$Defaults.init` validates its configured list
                // against an engine's suites during every server bootstrap.
                _ => Ok(Some(Value::Object(Some(jsse_supported_suite_name_array(
                    ctx,
                )?)))),
            }
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // FIX (httpserver-pkcs12-20260706): getSupportedCipherSuites/getSupportedProtocols
    // were never registered on javax/net/ssl/SSLEngine (only getEnabled* above), the
    // same "missing accessor" shape as the earlier SSLSocket
    // getSupportedCipherSuites/getEnabledCipherSuites gap (see
    // BUG-interfacedispatch-mbeanserver-sslsocket-realmode-shadow.md).
    // STALE AS WRITTEN, corrected 2026-08-12 (W7-61): "ssleng_alloc allocates every
    // SSLEngine directly on this abstract class (never a concrete subclass), so an
    // unregistered method here always throws AbstractMethodError on any real-JDK
    // caller" was true when it was written and is false now. In Compatible mode
    // `net_phase_e::register_re6_ssl_context` re-registers both createSSLEngine
    // descriptors AFTER this function runs (lib.rs 18191 then 18214) and allocates
    // `sun/security/ssl/SSLEngineImpl`, so `ssleng_alloc` allocates nothing there and
    // the Netty case below is served by `t27_tls::register_engine_impl_natives`
    // (lib.rs 18252), which registers `getSupportedCipherSuites` on SSLEngineImpl
    // itself. This registration is still the SYNTHETIC-mode answer, where
    // `register_phase68_natives` runs after `register_tls_natives` and wins. Leaving
    // the old sentence in place is what made W7-49 read this site as LIVE. Netty's
    // JdkSslContext.<clinit> (via
    // JdkSslContext$Defaults.init -> supportedCiphers) calls
    // SSLContext.getDefault().createSSLEngine().getSupportedCipherSuites() to validate
    // its configured cipher list against the engine's supported set — every
    // ServerHttpsRequestIntegrationTests run (Reactor Netty server backend) hits this
    // during server bootstrap.
    //
    // E42: the sentence that stood here — "this synthetic engine has no
    // negotiated-state distinction between 'supported' and 'enabled defaults'
    // beyond what's already modeled by the getEnabledCipherSuites default
    // branch above" — asserted a model that did not exist (that branch was one
    // hard-coded suite name), and its first clause is right for a reason it did
    // not give: HotSpot has no such distinction EITHER. Measured, enabled ==
    // supported, element-wise, on a fresh engine. Both accessors now answer
    // `t27_tls::SUPPORTED_CIPHER_SUITE_NAMES`, which is what this comment's
    // "mirrors the same fuller suite list" claim always meant to say — the
    // inline copy had drifted two entries behind it.
    r.register(
        ssleng,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(jsse_supported_suite_name_array(
                ctx,
            )?))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // NOTE: `getSupportedProtocols` is deliberately NOT
    // `JSSE_ENABLED_PROTOCOLS`. Measured (`scratchpad/e42/E42EnabledSets.java`)
    // HotSpot's supported list is strictly wider than its enabled one —
    // `[TLSv1.3, TLSv1.2, TLSv1.1, TLSv1, SSLv3, SSLv2Hello]` vs
    // `[TLSv1.3, TLSv1.2]` — so these two ARE a real distinction, unlike the
    // cipher pair above. This VM offers only the two, so the lists coincide
    // here by capability rather than by definition, and collapsing them into
    // one constant would erase the difference for whoever widens the VM.
    r.register(
        ssleng,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Single source of truth — see `t27_tls::SUPPORTED_PROTOCOL_NAMES`.
            let protos = crate::t27_tls::SUPPORTED_PROTOCOL_NAMES;
            // Typed and rooted (i6-L2) — see `jsse_supported_suite_name_array`.
            let arr = crate::t27_tls::materialize_java_string_array(ctx, protos);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(ssleng, "beginHandshake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // Transition from NOT_HANDSHAKING (0) to NEED_WRAP (1) for client, NEED_UNWRAP (2) for server
        let client = ctx.get_field(this, 0).as_int().unwrap_or(1);
        let new_status = if client != 0 { 1 } else { 2 };
        ctx.set_field(this, 5, Value::Int(new_status));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ssleng,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let status = ctx.get_field(this, 5).as_int().unwrap_or(0);
            let name = match status {
                0 => "NOT_HANDSHAKING",
                1 => "NEED_WRAP",
                2 => "NEED_UNWRAP",
                3 => "NEED_TASK",
                4 => "FINISHED",
                _ => "NOT_HANDSHAKING",
            };
            // Return an enum synthetic
            // gc-common w15-d: the name String first, then the holder (the
            // store went through the holder's pre-allocation address).
            let e: ObjectRef = p68_alloc_with_string_slot(
                ctx,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                2,
                0,
                name,
            )?;
            ctx.set_field(e, 1, Value::Int(status));
            Ok(Some(Value::Object(Some(e))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // T2.7.6 — SSLEngine.wrap(ByteBuffer src, ByteBuffer dst)
    // Drives the outbound state machine: if the handshake needs a WRAP,
    // we transition the state and return OK / NEED_UNWRAP. If not
    // handshaking, we copy `src` remaining bytes into `dst` as-is and
    // return OK / NOT_HANDSHAKING. The real TLS record framing happens
    // at the socket level (SSLSocket / SSLServerSocket) — this engine
    // path exists for callers like Netty that drive TLS through
    // ByteBuffer-level wrap/unwrap rather than getInputStream/getOutputStream.
    r.register(
        ssleng,
        "wrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        |ctx, args| {
            if let Some(real) = jdk_body_for_engine_subclass(ctx, args, "wrap") {
                return real;
            }
            let this = obj_arg(args, 0)?;
            let src = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.wrap: src is null".into()),
                    }
                    .into());
                }
            };
            let dst = match args.get(2) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.wrap: dst is null".into()),
                    }
                    .into());
                }
            };

            let hs = ctx.get_field(this, 5).as_int().unwrap_or(0);

            // Read src position / limit to figure out how many bytes to wrap.
            let src_pos = ctx.get_field(src, 1).as_int().unwrap_or(0) as usize;
            let src_lim = ctx.get_field(src, 2).as_int().unwrap_or(0) as usize;
            let remaining = if src_lim > src_pos {
                src_lim - src_pos
            } else {
                0
            };
            let dst_pos = ctx.get_field(dst, 1).as_int().unwrap_or(0) as usize;
            let dst_lim = ctx.get_field(dst, 2).as_int().unwrap_or(0) as usize;
            let dst_rem = if dst_lim > dst_pos {
                dst_lim - dst_pos
            } else {
                0
            };

            let bytes_produced;
            let bytes_consumed;
            let new_hs;

            if hs == 1 {
                // NEED_WRAP → produce a synthetic handshake record and
                // transition to NEED_UNWRAP. We write a minimal TLS
                // ClientHello-shaped header into dst so callers that inspect
                // the output don't see zero bytes.
                let fake_hello: &[u8] = &[
                    0x16, 0x03, 0x03, 0x00, 0x05, // TLS record header
                    0x01, 0x00, 0x00, 0x01, 0x03, // ClientHello stub
                ];
                let n = fake_hello.len().min(dst_rem);
                if let Value::Object(Some(dst_arr)) = ctx.get_field(dst, 0) {
                    for i in 0..n {
                        ctx.set_array_element(
                            dst_arr,
                            dst_pos + i,
                            Value::Int(fake_hello[i] as i8 as i32),
                        );
                    }
                }
                ctx.set_field(dst, 1, Value::Int((dst_pos + n) as i32));
                bytes_produced = n;
                bytes_consumed = 0;
                new_hs = 2; // → NEED_UNWRAP
            } else {
                // Normal data wrap: copy remaining from src → dst.
                let n = remaining.min(dst_rem);
                if n > 0 {
                    if let (Value::Object(Some(sa)), Value::Object(Some(da))) =
                        (ctx.get_field(src, 0), ctx.get_field(dst, 0))
                    {
                        for i in 0..n {
                            let v = ctx.get_array_element(sa, src_pos + i);
                            ctx.set_array_element(da, dst_pos + i, v);
                        }
                    }
                    ctx.set_field(src, 1, Value::Int((src_pos + n) as i32));
                    ctx.set_field(dst, 1, Value::Int((dst_pos + n) as i32));
                }
                bytes_produced = n;
                bytes_consumed = n;
                new_hs = 0; // NOT_HANDSHAKING
            }
            ctx.set_field(this, 5, Value::Int(new_hs));

            let result = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 4)?;
            // Fields: 0=status ordinal (OK=0), 1=handshakeStatus ordinal,
            //         2=bytesConsumed, 3=bytesProduced
            ctx.set_field(result, 0, Value::Int(0)); // OK
            ctx.set_field(result, 1, Value::Int(new_hs));
            ctx.set_field(result, 2, Value::Int(bytes_consumed as i32));
            ctx.set_field(result, 3, Value::Int(bytes_produced as i32));
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // T2.7.7 — SSLEngine.unwrap(ByteBuffer src, ByteBuffer dst)
    r.register(
        ssleng,
        "unwrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        |ctx, args| {
            if let Some(real) = jdk_body_for_engine_subclass(ctx, args, "unwrap") {
                return real;
            }
            let this = obj_arg(args, 0)?;
            let src = match args.get(1) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.unwrap: src is null".into()),
                    }
                    .into());
                }
            };
            let dst = match args.get(2) {
                Some(Value::Object(Some(o))) => *o,
                _ => {
                    return Err(RuntimeError::NullPointerException {
                        message: Some("SSLEngine.unwrap: dst is null".into()),
                    }
                    .into());
                }
            };

            let hs = ctx.get_field(this, 5).as_int().unwrap_or(0);
            let src_pos = ctx.get_field(src, 1).as_int().unwrap_or(0) as usize;
            let src_lim = ctx.get_field(src, 2).as_int().unwrap_or(0) as usize;
            let remaining = if src_lim > src_pos {
                src_lim - src_pos
            } else {
                0
            };
            let dst_pos = ctx.get_field(dst, 1).as_int().unwrap_or(0) as usize;
            let dst_lim = ctx.get_field(dst, 2).as_int().unwrap_or(0) as usize;
            let dst_rem = if dst_lim > dst_pos {
                dst_lim - dst_pos
            } else {
                0
            };

            let bytes_produced;
            let bytes_consumed;
            let new_hs;

            if hs == 2 {
                // NEED_UNWRAP → consume src (simulated server response) and
                // transition to FINISHED. In a real rustls pipeline the
                // ServerHello + EncryptedExtensions + Finished would be parsed
                // here; our synthetic engine skips that because the actual TLS
                // record processing occurs inside the SSLSocket / rustls
                // StreamOwned path.
                let n = remaining.min(64); // consume up to 64 bytes
                if n > 0 {
                    ctx.set_field(src, 1, Value::Int((src_pos + n) as i32));
                }
                bytes_consumed = n;
                bytes_produced = 0;
                new_hs = 4; // FINISHED
            } else {
                // Normal data unwrap: copy src → dst.
                let n = remaining.min(dst_rem);
                if n > 0 {
                    if let (Value::Object(Some(sa)), Value::Object(Some(da))) =
                        (ctx.get_field(src, 0), ctx.get_field(dst, 0))
                    {
                        for i in 0..n {
                            let v = ctx.get_array_element(sa, src_pos + i);
                            ctx.set_array_element(da, dst_pos + i, v);
                        }
                    }
                    ctx.set_field(src, 1, Value::Int((src_pos + n) as i32));
                    ctx.set_field(dst, 1, Value::Int((dst_pos + n) as i32));
                }
                bytes_consumed = n;
                bytes_produced = n;
                new_hs = 0; // NOT_HANDSHAKING
            }
            ctx.set_field(this, 5, Value::Int(new_hs));

            let result = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 4)?;
            ctx.set_field(result, 0, Value::Int(0)); // OK
            ctx.set_field(result, 1, Value::Int(new_hs));
            ctx.set_field(result, 2, Value::Int(bytes_consumed as i32));
            ctx.set_field(result, 3, Value::Int(bytes_produced as i32));
            Ok(Some(Value::Object(Some(result))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // SSLEngineResult accessors.
    //
    // FIX (sslengineresult-stub-shadow): these are the ACTIVE accessor
    // overrides for `javax/net/ssl/SSLEngineResult` — `register_p68_ssl`
    // runs on the real-JDK path too since the httpserver-pkcs12 fix
    // (lib.rs), and registration is last-wins, so this copy shadows BOTH
    // the real `SSLEngineResult` bytecode AND the older duplicate in
    // `tls.rs::register_ssl_engine_result`. The pre-fix bodies assumed
    // every receiver was this file's synthetic int-code result and rebuilt
    // a FRESH 2-slot fake enum (name@0, ordinal@1) on every call — a real
    // result object's enum-reference field read through `.as_int()`
    // defaulted to 0, so `getStatus()`/`getHandshakeStatus()` returned
    // "OK"/"NOT_HANDSHAKING"-named fakes that `==`-match no real singleton.
    // Real JSSE drivers gate their handshake state machines on exactly
    // those identity comparisons (e.g. `sun.net.httpserver.SSLStreams
    // .recvData`'s `hs == FINISHED / hs == NOT_HANDSHAKING` checks before
    // `doHandshake`), so a real-JDK `HttpsServer` never learned it had to
    // WRAP after consuming the ClientHello: it busy-spun re-reading a
    // socket that would never deliver more bytes while the client timed
    // out with `SSLHandshakeException: handshake read: Resource
    // temporarily unavailable (os error 11)` — deterministically, for
    // every `com.sun.net.httpserver.HttpsServer` exchange.
    //
    // Now: a real enum reference stored on the receiver (results built via
    // the real 4-arg ctor in `t27_tls::alloc_engine_result`) passes
    // through untouched — by-name first (layout-proof), then the raw slot.
    // Int-code receivers (this file's fake-engine producers above, which
    // use the name tables below) resolve the REAL enum singleton via
    // `valueOf` so identity comparisons hold even for synthetic results;
    // only when the real enum class is unresolvable (pure synthetic-JDK
    // mode) do they fall back to the legacy fresh 2-slot holder.
    let ssleng_result = "javax/net/ssl/SSLEngineResult";
    r.register(
        ssleng_result,
        "getStatus",
        "()Ljavax/net/ssl/SSLEngineResult$Status;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(o)) = ctx.get_field_by_name(this, "status") {
                return Ok(Some(Value::Object(Some(o))));
            }
            let raw = ctx.get_field(this, 0);
            if let Value::Object(Some(_)) = raw {
                return Ok(Some(raw));
            }
            let status = raw.as_int().unwrap_or(0);
            let name = match status {
                0 => "OK",
                1 => "BUFFER_UNDERFLOW",
                2 => "BUFFER_OVERFLOW",
                3 => "CLOSED",
                _ => "OK",
            };
            let arg = ctx.create_string(name);
            if let Ok(Some(v)) = ctx.invoke(
                "javax/net/ssl/SSLEngineResult$Status",
                "valueOf",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLEngineResult$Status;",
                &[Value::Object(Some(arg))],
            ) {
                if matches!(v, Value::Object(Some(_))) {
                    return Ok(Some(v));
                }
            }
            // gc-common w15-d: the name String first, then the holder.
            let e: ObjectRef = p68_alloc_with_string_slot(
                ctx,
                "javax/net/ssl/SSLEngineResult$Status",
                2,
                0,
                name,
            )?;
            ctx.set_field(e, 1, Value::Int(status));
            Ok(Some(Value::Object(Some(e))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ssleng_result,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Value::Object(Some(o)) = ctx.get_field_by_name(this, "handshakeStatus") {
                return Ok(Some(Value::Object(Some(o))));
            }
            let raw = ctx.get_field(this, 1);
            if let Value::Object(Some(_)) = raw {
                return Ok(Some(raw));
            }
            let status = raw.as_int().unwrap_or(0);
            let name = match status {
                0 => "NOT_HANDSHAKING",
                1 => "NEED_WRAP",
                2 => "NEED_UNWRAP",
                3 => "NEED_TASK",
                4 => "FINISHED",
                _ => "NOT_HANDSHAKING",
            };
            let arg = ctx.create_string(name);
            if let Ok(Some(v)) = ctx.invoke(
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                "valueOf",
                "(Ljava/lang/String;)Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
                &[Value::Object(Some(arg))],
            ) {
                if matches!(v, Value::Object(Some(_))) {
                    return Ok(Some(v));
                }
            }
            // gc-common w15-d: the name String first, then the holder (the
            // store went through the holder's pre-allocation address).
            let e: ObjectRef = p68_alloc_with_string_slot(
                ctx,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                2,
                0,
                name,
            )?;
            ctx.set_field(e, 1, Value::Int(status));
            Ok(Some(Value::Object(Some(e))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // bytesConsumed/bytesProduced: real field by name first (layout-proof
    // for real-ctor results regardless of declaration order), then the raw
    // synthetic slot convention (consumed@2, produced@3).
    r.register(ssleng_result, "bytesConsumed", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(n) = ctx.get_field_by_name(this, "bytesConsumed") {
            return Ok(Some(Value::Int(n)));        }
        Ok(Some(ctx.get_field(this, 2)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng_result, "bytesProduced", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        if let Value::Int(n) = ctx.get_field_by_name(this, "bytesProduced") {
            return Ok(Some(Value::Int(n)));
        }
        Ok(Some(ctx.get_field(this, 3)))
    }, cratonvm_native_api::NativeKind::Bridge);

    r.register(ssleng, "closeOutbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 5, Value::Int(4)); // FINISHED
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "closeInbound", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        ctx.set_field(this, 5, Value::Int(4));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "isOutboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let status = ctx.get_field(this, 5).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if status == 4 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ssleng, "isInboundDone", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let status = ctx.get_field(this, 5).as_int().unwrap_or(0);
        Ok(Some(Value::Int(if status == 4 { 1 } else { 0 })))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        ssleng,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Return cached session if present
            if let Value::Object(Some(s)) = ctx.get_field(this, 6) {
                return Ok(Some(Value::Object(Some(s))));
            }
            // Build a fresh session and cache it.
            //
            // E12: this engine has NO backing TLS connection at all — slot 6
            // is the only session state it has, and reaching this arm means
            // nothing has been negotiated. It used to report `TLSv1.3` /
            // `TLS_AES_128_GCM_SHA256`: a specific, strong, *supported* suite
            // for a handshake that never happened. HotSpot's answer for the
            // same state is measured in arm B of
            // `scratchpad/e12/E12SessionContract.java` — `SSL_NULL_WITH_NULL_NULL`
            // / `NONE` — and unlike the two literals above, that pair cannot
            // be confused with a real negotiation, because JSSE refuses to
            // enable it (`setEnabledCipherSuites("SSL_NULL_WITH_NULL_NULL")`
            // throws `IllegalArgumentException`). See `JSSE_NULL_CIPHER_SUITE`.
            //
            // E42: this used to mint a bespoke 2-FIELD session — a fifth
            // `javax/net/ssl/SSLSession` width whose only difference from the
            // null session next door was that it had no stream-id slot and no
            // attribute slot. It is the same object in the same state, so it is
            // now the same constructor: `new13_alloc_null_ssl_session(-1)`,
            // which writes the identical (protocol, cipher) sentinel pair, pins
            // across both `create_string` GC points (this site did not), and
            // carries `NEW13_SESS_ATTRS` so a `putValue` on a pre-handshake
            // engine session round-trips instead of silently vanishing.
            //
            // Every `t27_tls` width rule answers the same for both widths, so
            // no accessor changes: `session_proto_slot` = 0 and
            // `session_cipher_slot` = 1 at width 2 and at width 4;
            // `session_has_negotiated` answered `false` at width 2 via the
            // `0..=2` arm and answers `false` at width 4 by reading
            // `NEW13_SESS_TLSID = -1` (the co-requisite arm merge —
            // see `NEW13_SSL_SESS_FIELDS`). Two ways of saying "nothing was
            // negotiated" collapse into one.
            //
            // Slot order is (proto, cipher), matching the `< 6 fields` arm of
            // `t27_tls`'s field-count disambiguation as well as
            // `NEW13_SESS_PROTO`/`_CIPHER`.
            //
            // `this` is PINNED across the constructor: it allocates a session
            // and two Strings, each a GC point, and the receiver is written
            // afterwards. The old code had the same three allocation points
            // and did not pin, so the cache write below could land through a
            // stale reference under a moving young collection — the hazard
            // `new13_alloc_null_ssl_session`'s own comment describes for the
            // session object, applied to the engine that owns it.
            let this_pin = ctx.pin_native_root(this);
            let session = new13_alloc_null_ssl_session(ctx, -1)?;
            let this = ctx.read_native_pin(this_pin, this);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field(this, 6, Value::Object(Some(session)));
            Ok(Some(Value::Object(Some(session))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
}

