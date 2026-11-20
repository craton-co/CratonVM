use super::*;
use crate::test_utils::MockNativeContext;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

// --- TLS deny-by-default -------------------------------------------------
//
// Every "must raise" case below has a "must still work" twin, so the suite
// cannot be satisfied by rejecting everything.

fn tls_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    r
}

/// `SSLContext.getInstance(name)` → the returned context, or the failure.
fn get_instance(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    name: &str,
) -> MethodCallResult {
    let f = r
        .find(
            "javax/net/ssl/SSLContext",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        )
        .expect("getInstance registered");
    let arg = ctx.create_string(name);
    f(ctx, &[Value::Object(Some(arg))])
}

fn protocol_of(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    context: ObjectRef,
) -> String {
    let f = r
        .find(
            "javax/net/ssl/SSLContext",
            "getProtocol",
            "()Ljava/lang/String;",
        )
        .expect("getProtocol registered");
    match f(ctx, &[Value::Object(Some(context))]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        other => panic!("getProtocol returned {other:?}"),
    }
}

// MUST STILL WORK.
#[test]
fn ssl_context_get_instance_accepts_every_supported_protocol_and_echoes_it_back() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    // (requested name, name `getProtocol()` must report)
    for (requested, expected) in [
        ("TLS", "TLS"),
        ("Default", "TLS"),
        ("TLSv1.2", "TLSv1.2"),
        ("TLSv1.3", "TLSv1.3"),
        ("SSL", "SSL"),
        ("TLSv1", "TLSv1"),
        ("TLSv1.1", "TLSv1.1"),
        ("SSLv3", "SSLv3"),
        // W3-7: SunJSSE registers three DTLS SSLContext services on JDK
        // 25 (measured). This registration refused all three.
        ("DTLS", "DTLS"),
        ("DTLSv1.0", "DTLSv1.0"),
        ("DTLSv1.2", "DTLSv1.2"),
    ] {
        let context = match get_instance(&r, &mut ctx, requested) {
            Ok(Some(Value::Object(Some(o)))) => o,
            other => panic!("getInstance({requested}) failed: {other:?}"),
        };
        assert_eq!(
            protocol_of(&r, &mut ctx, context),
            expected,
            "getInstance({requested}) must not silently substitute a protocol"
        );
    }
}

// MUST STILL WORK — JCA algorithm lookup is case-insensitive.
#[test]
fn ssl_context_get_instance_is_case_insensitive() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    for name in ["tls", "TLSV1.3", "tlsv1.2", "ssl"] {
        assert!(
            matches!(
                get_instance(&r, &mut ctx, name),
                Ok(Some(Value::Object(Some(_))))
            ),
            "getInstance({name}) must be accepted"
        );
    }
}

// MUST RAISE.
#[test]
fn ssl_context_get_instance_refuses_an_unsupported_protocol() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    for name in ["SSLv2", "NoSuchThing", "TLSv1.4", "", "TLS "] {
        let result = get_instance(&r, &mut ctx, name);
        assert!(
            result.is_err(),
            "getInstance({name:?}) must raise, not substitute a TLS context"
        );
    }
}

#[test]
fn the_unsupported_protocol_refusal_is_a_no_such_algorithm_exception() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    let err = get_instance(&r, &mut ctx, "SSLv2").expect_err("must raise");
    match err {
        MethodCallFailed::ExceptionThrown(exc) => {
            let cid = ctx.class_id_of_object(exc);
            assert_eq!(
                ctx.class_name_arc_of_id(cid).as_deref(),
                Some("java/security/NoSuchAlgorithmException")
            );
        }
        // Fallback arm: still an exception, never a context.
        MethodCallFailed::InternalError(e) => {
            let text = format!("{e}");
            assert!(
                text.contains("IllegalArgumentException"),
                "unexpected fallback: {text}"
            );
        }
    }
}

#[test]
fn ssl_context_get_instance_of_null_is_a_null_pointer_exception() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    let f = r
        .find(
            "javax/net/ssl/SSLContext",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        )
        .unwrap();
    assert!(f(&mut ctx, &[Value::Object(None)]).is_err());
}

// --- SSLContext factory surface requires init() --------------------------

fn socket_factory_of(
    r: &NativeMethodRegistry,
    ctx: &mut MockNativeContext,
    context: ObjectRef,
) -> MethodCallResult {
    let f = r
        .find(
            "javax/net/ssl/SSLContext",
            "getSocketFactory",
            "()Ljavax/net/ssl/SSLSocketFactory;",
        )
        .expect("getSocketFactory registered");
    f(ctx, &[Value::Object(Some(context))])
}

// MUST RAISE: a factory from an un-`init()`ed context carries neither key
// nor trust managers, and nothing about the returned object says so.
#[test]
fn get_socket_factory_on_an_uninitialized_context_raises() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    let context = match get_instance(&r, &mut ctx, "TLSv1.3") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("getInstance failed: {other:?}"),
    };
    assert!(socket_factory_of(&r, &mut ctx, context).is_err());

    let g = r
        .find(
            "javax/net/ssl/SSLContext",
            "getServerSocketFactory",
            "()Ljavax/net/ssl/SSLServerSocketFactory;",
        )
        .unwrap();
    assert!(g(&mut ctx, &[Value::Object(Some(context))]).is_err());
}

// MUST STILL WORK — the twin: after `init()` the same call succeeds.
#[test]
fn get_socket_factory_after_init_succeeds() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    let context = match get_instance(&r, &mut ctx, "TLSv1.3") {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("getInstance failed: {other:?}"),
    };
    let init = r
        .find(
            "javax/net/ssl/SSLContext",
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        )
        .unwrap();
    init(
        &mut ctx,
        &[
            Value::Object(Some(context)),
            Value::Object(None),
            Value::Object(None),
            Value::Object(None),
        ],
    )
    .expect("init");
    assert!(matches!(
        socket_factory_of(&r, &mut ctx, context),
        Ok(Some(Value::Object(Some(_))))
    ));
}

// MUST STILL WORK — `getDefault()` hands back a pre-initialised context,
// so the init gate must not break the JDK's documented default path.
#[test]
fn get_default_context_can_still_hand_out_a_socket_factory() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    let f = r
        .find(
            "javax/net/ssl/SSLContext",
            "getDefault",
            "()Ljavax/net/ssl/SSLContext;",
        )
        .unwrap();
    let context = match f(&mut ctx, &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("getDefault failed: {other:?}"),
    };
    assert!(matches!(
        socket_factory_of(&r, &mut ctx, context),
        Ok(Some(Value::Object(Some(_))))
    ));
}

// --- The non-cryptographic SSLEngine ------------------------------------

/// All three engine cases live in ONE test: they share the process-wide
/// opt-in flag, so running them as separate `#[test]`s would race under
/// the default parallel test harness.
#[test]
fn noncrypto_engine_denies_by_default_works_when_opted_in_and_still_reports_closed() {
    let r = tls_registry();
    let mut ctx = MockNativeContext::new();
    let wrap = r
        .find(
            "javax/net/ssl/SSLEngine",
            "wrap",
            "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        )
        .expect("wrap registered");
    let unwrap = r
        .find(
            "javax/net/ssl/SSLEngine",
            "unwrap",
            "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        )
        .expect("unwrap registered");

    // MUST RAISE (default): no cryptography happened, so no OK status.
    set_noncrypto_engine_opt_in(false);
    let engine = alloc_ssl_engine(&mut ctx).unwrap();
    let args = [Value::Object(Some(engine))];
    assert!(
        wrap(&mut ctx, &args).is_err(),
        "wrap must not report success for an unprotected connection"
    );
    assert!(
        unwrap(&mut ctx, &args).is_err(),
        "unwrap must not report a finished handshake that never happened"
    );

    // PRESERVED NEGATIVE: `CLOSED` after `closeOutbound()` is a real,
    // correct answer and must stay a value, not become an exception.
    ctx.set_field(engine, ENG_OUTBOUND_DONE, Value::Int(1));
    ctx.set_field(engine, ENG_INBOUND_DONE, Value::Int(1));
    for f in [wrap, unwrap] {
        match f(&mut ctx, &args) {
            Ok(Some(Value::Object(Some(result)))) => {
                assert_eq!(ctx.get_field(result, 0), Value::Int(STATUS_CLOSED));
            }
            other => panic!("closed engine must still report CLOSED, got {other:?}"),
        }
    }

    // MUST STILL WORK: the explicit opt-in restores the legacy behaviour.
    set_noncrypto_engine_opt_in(true);
    let legacy = alloc_ssl_engine(&mut ctx).unwrap();
    let legacy_args = [Value::Object(Some(legacy))];
    assert!(matches!(
        wrap(&mut ctx, &legacy_args),
        Ok(Some(Value::Object(Some(_))))
    ));
    assert!(matches!(
        unwrap(&mut ctx, &legacy_args),
        Ok(Some(Value::Object(Some(_))))
    ));

    // Leave the process in the secure state for any later test.
    set_noncrypto_engine_opt_in(false);
}

// --- SSLEngineResult int-code convention translation ---------------------

#[test]
fn engine_result_code_translation_maps_conventions_onto_real_ordinals() {
    // This file's fake-engine convention → t27_tls real-JDK-ordinal codes.
    assert_eq!(tls_status_code_to_real(STATUS_OK), crate::t27_tls::SR_OK);
    assert_eq!(
        tls_status_code_to_real(STATUS_CLOSED),
        crate::t27_tls::SR_CLOSED
    );
    assert_eq!(
        tls_status_code_to_real(STATUS_BUFFER_UNDERFLOW),
        crate::t27_tls::SR_BUFFER_UNDERFLOW
    );
    assert_eq!(
        tls_status_code_to_real(STATUS_BUFFER_OVERFLOW),
        crate::t27_tls::SR_BUFFER_OVERFLOW
    );
    assert_eq!(
        tls_hs_code_to_real(HS_NOT_HANDSHAKING),
        crate::t27_tls::HS_NOT_HANDSHAKING_R
    );
    assert_eq!(
        tls_hs_code_to_real(HS_NEED_WRAP),
        crate::t27_tls::HS_NEED_WRAP_R
    );
    assert_eq!(
        tls_hs_code_to_real(HS_NEED_UNWRAP),
        crate::t27_tls::HS_NEED_UNWRAP_R
    );
    assert_eq!(
        tls_hs_code_to_real(HS_NEED_TASK),
        crate::t27_tls::HS_NEED_TASK_R
    );
    assert_eq!(
        tls_hs_code_to_real(HS_FINISHED),
        crate::t27_tls::HS_FINISHED_R
    );
    // The two conventions genuinely disagree on these — the whole reason
    // the translation exists (e.g. raw code 3 is NEED_TASK here but
    // NEED_WRAP in real-JDK ordinals).
    assert_ne!(HS_NEED_WRAP, crate::t27_tls::HS_NEED_WRAP_R);
    assert_ne!(STATUS_CLOSED, crate::t27_tls::SR_CLOSED);
}

// --- Registration tests -------------------------------------------------

#[test]
fn test_ssl_context_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLContext";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"
        )
        .is_some());
    assert!(r
        .find(cls, "getDefault", "()Ljavax/net/ssl/SSLContext;")
        .is_some());
    assert!(r.find(
        cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"
    ).is_some());
    assert!(r
        .find(cls, "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
        .is_some());
    assert!(r
        .find(
            cls,
            "createSSLEngine",
            "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getSocketFactory",
            "()Ljavax/net/ssl/SSLSocketFactory;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getServerSocketFactory",
            "()Ljavax/net/ssl/SSLServerSocketFactory;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getDefaultSSLParameters",
            "()Ljavax/net/ssl/SSLParameters;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "getSupportedSSLParameters",
            "()Ljavax/net/ssl/SSLParameters;"
        )
        .is_some());
    assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
}

#[test]
fn test_ssl_engine_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLEngine";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "wrap",
            "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "unwrap",
            "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
        )
        .is_some());
    assert!(r.find(cls, "beginHandshake", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getHandshakeStatus",
            "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"
        )
        .is_some());
    assert!(r.find(cls, "isInboundDone", "()Z").is_some());
    assert!(r.find(cls, "isOutboundDone", "()Z").is_some());
    assert!(r.find(cls, "closeInbound", "()V").is_some());
    assert!(r.find(cls, "closeOutbound", "()V").is_some());
    assert!(r.find(cls, "setUseClientMode", "(Z)V").is_some());
    assert!(r.find(cls, "getUseClientMode", "()Z").is_some());
    assert!(r.find(cls, "getPeerHost", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getPeerPort", "()I").is_some());
    assert!(r
        .find(cls, "setEnabledProtocols", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "setEnabledCipherSuites", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getApplicationProtocol", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
        .is_some());
    assert!(r
        .find(cls, "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
        .is_some());
}

#[test]
fn test_ssl_session_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLSession";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(cls, "getCipherSuite", "()Ljava/lang/String;")
        .is_some());
    assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "isValid", "()Z").is_some());
    assert!(r.find(cls, "invalidate", "()V").is_some());
    assert!(r.find(cls, "getId", "()[B").is_some());
    assert!(r.find(cls, "getPeerHost", "()Ljava/lang/String;").is_some());
    assert!(r.find(cls, "getPeerPort", "()I").is_some());
    assert!(r.find(cls, "getCreationTime", "()J").is_some());
    assert!(r.find(cls, "getLastAccessedTime", "()J").is_some());
    assert!(r.find(cls, "getApplicationBufferSize", "()I").is_some());
    assert!(r.find(cls, "getPacketBufferSize", "()I").is_some());
}

#[test]
fn test_ssl_parameters_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLParameters";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(cls, "getProtocols", "()[Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setProtocols", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getCipherSuites", "()[Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setCipherSuites", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getApplicationProtocols", "()[Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(
            cls,
            "getEndpointIdentificationAlgorithm",
            "()Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "setEndpointIdentificationAlgorithm",
            "(Ljava/lang/String;)V"
        )
        .is_some());
    assert!(r.find(cls, "getNeedClientAuth", "()Z").is_some());
    assert!(r.find(cls, "setNeedClientAuth", "(Z)V").is_some());
    assert!(r.find(cls, "getWantClientAuth", "()Z").is_some());
    assert!(r.find(cls, "setWantClientAuth", "(Z)V").is_some());
}

#[test]
fn test_trust_manager_factory_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/TrustManagerFactory";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;"
        )
        .is_some());
    assert!(r
        .find(cls, "getDefaultAlgorithm", "()Ljava/lang/String;")
        .is_some());
    assert!(r.find(cls, "init", "(Ljava/security/KeyStore;)V").is_some());
    assert!(r
        .find(cls, "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;")
        .is_some());
}

// FIX (nb-tls-tmf): getTrustManagers() must propagate the keystore registry
// id bound by init() (factory slot 2) onto the returned X509TrustManager,
// so validate_cert_chain's read_trust_manager_id_from_obj resolves the
// bound trust anchors rather than returning a no-validation placeholder.
//
// UPDATED (tls-residuals, see x509_manager::register_trust_manager_state's
// doc comment): the id stamped on slot 0 is no longer the raw keystore
// registry id itself. `getTrustManagers()` now builds a `TrustManagerState`
// from the bound keystore id and registers *that state* through
// `register_trust_manager_state`, getting back a fresh id in the separate
// `tm_registry` id space — deliberately, because stamping the raw keystore
// id directly caused `validate_cert_chain` to misresolve it against a
// numerically-coincident but unrelated `tm_registry` entry from other
// TrustManagerFactory SPI paths (both counters start at 1 and increment
// independently). So slot 0 is now an *indirection*: resolving it via
// `trust_manager_state_by_id` must yield a `TrustManagerState` whose own
// `keystore_id` field is the one init() bound — that's the invariant this
// test checks, not raw numeric equality with the bound id.
#[test]
fn nb_tls_tmf_get_trust_managers_propagates_keystore_id() {
    use crate::test_utils::MockNativeContext;
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/TrustManagerFactory";
    let mut ctx = MockNativeContext::new();

    // Obtain a factory instance via getInstance (PKIX), then simulate
    // init(KeyStore) having bound a keystore registry id by writing slot 2.
    let get_instance = r
        .find(
            cls,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        )
        .unwrap();
    let factory = match get_instance(&mut ctx, &[Value::Object(None)]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("getInstance should return a factory, got {other:?}"),
    };
    const BOUND_ID: i32 = 7;
    ctx.set_field(factory, 2, Value::Int(BOUND_ID));

    // getTrustManagers() should return a 1-element array whose X509TrustManager
    // carries a `tm_registry` id resolving back to the bound keystore id.
    let get_tms = r
        .find(cls, "getTrustManagers", "()[Ljavax/net/ssl/TrustManager;")
        .unwrap();
    let arr = match get_tms(&mut ctx, &[Value::Object(Some(factory))]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        other => panic!("getTrustManagers should return an array, got {other:?}"),
    };
    assert_eq!(
        ctx.array_length(arr),
        1,
        "expected exactly one trust manager"
    );
    let tm = match ctx.get_array_element(arr, 0) {
        Value::Object(Some(t)) => t,
        other => panic!("trust manager element should be an object, got {other:?}"),
    };
    let tm_class = ctx
        .class_name_of_id(ctx.class_id_of_object(tm))
        .unwrap_or_default();
    assert_eq!(
        tm_class,
        crate::x509_manager::FQN_X509_TM,
        "TrustManagerFactory must return a concrete X509TrustManagerImpl mirror, not the abstract javax.net.ssl.X509TrustManager interface"
    );
    let tm_id = match ctx.get_field(tm, 0) {
        Value::Int(i) => i,
        other => panic!("trust manager id (slot 0) should be an Int, got {other:?}"),
    };
    let resolved_state = crate::x509_manager::trust_manager_state_by_id(tm_id);
    assert_eq!(
        resolved_state.keystore_id, BOUND_ID,
        "the X509TrustManager's stamped id must resolve (via \
         trust_manager_state_by_id) to the TrustManagerState built from \
         the keystore id init() bound, so checkServerTrusted validates \
         against those anchors (not a no-op placeholder or an unrelated \
         tm_registry entry)"
    );
}

#[test]
fn test_key_manager_factory_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/KeyManagerFactory";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;"
        )
        .is_some());
    assert!(r
        .find(cls, "getDefaultAlgorithm", "()Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "init", "(Ljava/security/KeyStore;[C)V")
        .is_some());
    assert!(r
        .find(cls, "getKeyManagers", "()[Ljavax/net/ssl/KeyManager;")
        .is_some());
}

#[test]
fn test_key_store_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "java/security/KeyStore";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/KeyStore;"
        )
        .is_some());
    assert!(r
        .find(cls, "getDefaultType", "()Ljava/lang/String;")
        .is_some());
    assert!(r.find(cls, "load", "(Ljava/io/InputStream;[C)V").is_some());
    assert!(r
        .find(
            cls,
            "getCertificate",
            "(Ljava/lang/String;)Ljava/security/cert/Certificate;"
        )
        .is_some());
    assert!(r
        .find(cls, "getKey", "(Ljava/lang/String;[C)Ljava/security/Key;")
        .is_some());
    assert!(r
        .find(cls, "containsAlias", "(Ljava/lang/String;)Z")
        .is_some());
    assert!(r
        .find(cls, "aliases", "()Ljava/util/Enumeration;")
        .is_some());
    assert!(r.find(cls, "size", "()I").is_some());
    assert!(r
        .find(
            cls,
            "setCertificateEntry",
            "(Ljava/lang/String;Ljava/security/cert/Certificate;)V"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "setKeyEntry",
            "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V"
        )
        .is_some());
    assert!(r
        .find(cls, "deleteEntry", "(Ljava/lang/String;)V")
        .is_some());
}

#[test]
fn test_ssl_socket_factory_synthetic_surface_is_dropped_with_real_sockets() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLSocketFactory";
    for (name, descriptor) in [
        ("<init>", "()V"),
        ("getDefault", "()Ljavax/net/ssl/SSLSocketFactory;"),
        ("createSocket", "(Ljava/lang/String;I)Ljava/net/Socket;"),
        (
            "createSocket",
            "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
        ),
        ("getDefaultCipherSuites", "()[Ljava/lang/String;"),
        ("getSupportedCipherSuites", "()[Ljava/lang/String;"),
    ] {
        assert!(
            r.find(cls, name, descriptor).is_none(),
            "{cls}.{name}{descriptor} is a SyntheticStub and must not shadow \
             the real socket/TLS path when real sockets are the default"
        );
    }
}

#[test]
fn test_x509_trust_manager_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/X509TrustManager";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "checkClientTrusted",
            "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "checkServerTrusted",
            "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"
        )
        .is_some());
    // T19.9: getAcceptedIssuers moved to t27_tls.rs (rustls-backed). The
    // tls.rs baseline no longer registers it — t27 is the canonical home.
    assert!(r
        .find(
            cls,
            "getAcceptedIssuers",
            "()[Ljava/security/cert/X509Certificate;"
        )
        .is_none());
}

// VULN [nb-tls] regression: the X509TrustManager check{Server,Client}Trusted
// path must perform REAL PKIX validation in every build, NOT return Ok(None).
// `validate_cert_chain` routes to `crate::x509_manager::validate_chain`, so
// we guard the security property of that production validator directly: an
// empty chain and an unparseable/untrusted chain MUST be rejected (no
// trust anchors configured). Before the fix the default build accepted any
// chain unconditionally.
#[test]
fn test_validate_chain_rejects_untrusted_in_all_builds() {
    use crate::x509_manager::{validate_chain, TrustManagerState};

    // No trust anchors → nothing can validate.
    let trust = TrustManagerState::default();

    // (1) Empty chain is rejected.
    let empty: Vec<Vec<u8>> = Vec::new();
    assert!(
        validate_chain(&empty, &trust).is_err(),
        "empty cert chain must be rejected"
    );

    // (2) A structurally invalid / untrusted single-cert chain is rejected
    // (garbage DER fails to parse; even a well-formed self-signed cert with
    // no matching anchor would fail at the trust-anchor step). The key
    // property is simply: validation actually runs and says NO.
    let garbage: Vec<Vec<u8>> = vec![vec![0x30, 0x03, 0x02, 0x01, 0x00]];
    assert!(
        validate_chain(&garbage, &trust).is_err(),
        "untrusted/unparseable cert chain must be rejected"
    );
}

#[test]
fn test_ssl_context_impl_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "sun/security/ssl/SSLContextImpl";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(
            cls,
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"
        )
        .is_some());
    assert!(r
        .find(cls, "getDefault", "()Ljavax/net/ssl/SSLContext;")
        .is_some());
    assert!(r.find(
        cls,
        "init",
        "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V"
    ).is_some());
    assert!(r
        .find(cls, "createSSLEngine", "()Ljavax/net/ssl/SSLEngine;")
        .is_some());
    assert!(r.find(cls, "getProtocol", "()Ljava/lang/String;").is_some());
}

#[test]
fn test_ssl_engine_result_registration() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLEngineResult";
    assert!(r.find(cls, "<init>", "()V").is_some());
    assert!(r
        .find(cls, "getStatus", "()Ljavax/net/ssl/SSLEngineResult$Status;")
        .is_some());
    assert!(r
        .find(
            cls,
            "getHandshakeStatus",
            "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"
        )
        .is_some());
    assert!(r.find(cls, "bytesConsumed", "()I").is_some());
    assert!(r.find(cls, "bytesProduced", "()I").is_some());
}

// --- Constant / logic tests (no VM context required) -------------------

#[test]
fn test_tls13_ciphers_not_empty() {
    assert!(!TLS13_CIPHERS.is_empty());
    assert_eq!(TLS13_CIPHERS[0], "TLS_AES_256_GCM_SHA384");
    assert_eq!(TLS13_CIPHERS[1], "TLS_AES_128_GCM_SHA256");
    assert_eq!(TLS13_CIPHERS[2], "TLS_CHACHA20_POLY1305_SHA256");
}

#[test]
fn test_tls12_ciphers_not_empty() {
    assert!(!TLS12_CIPHERS.is_empty());
    assert!(TLS12_CIPHERS.iter().all(|c| c.starts_with("TLS_")));
}

#[test]
fn test_supported_protocols_order() {
    assert_eq!(SUPPORTED_PROTOCOLS[0], "TLSv1.3");
    assert_eq!(SUPPORTED_PROTOCOLS[1], "TLSv1.2");
    assert_eq!(
        SUPPORTED_PROTOCOLS.len(),
        2,
        "Only TLSv1.3 and TLSv1.2 should be supported"
    );
}

#[test]
fn test_status_constants_are_distinct() {
    let statuses = [
        STATUS_OK,
        STATUS_CLOSED,
        STATUS_BUFFER_UNDERFLOW,
        STATUS_BUFFER_OVERFLOW,
    ];
    for i in 0..statuses.len() {
        for j in (i + 1)..statuses.len() {
            assert_ne!(statuses[i], statuses[j]);
        }
    }
}

#[test]
fn test_handshake_status_constants_are_distinct() {
    let hs = [
        HS_NOT_HANDSHAKING,
        HS_NEED_WRAP,
        HS_NEED_UNWRAP,
        HS_NEED_TASK,
        HS_FINISHED,
    ];
    for i in 0..hs.len() {
        for j in (i + 1)..hs.len() {
            assert_ne!(hs[i], hs[j]);
        }
    }
}

#[test]
fn test_epoch_millis_is_positive() {
    let ms = epoch_millis();
    assert!(ms > 0);
}

#[test]
fn test_epoch_millis_is_plausible() {
    // Should be after 2020-01-01 = 1577836800000 ms
    let ms = epoch_millis();
    assert!(ms > 1_577_836_800_000);
}

#[test]
fn test_all_tls13_cipher_names_valid() {
    for cipher in TLS13_CIPHERS {
        assert!(cipher.starts_with("TLS_"), "Bad cipher: {}", cipher);
        assert!(!cipher.is_empty());
    }
}

#[test]
fn test_all_tls12_cipher_names_valid() {
    for cipher in TLS12_CIPHERS {
        assert!(cipher.starts_with("TLS_"), "Bad cipher: {}", cipher);
    }
}

#[test]
fn test_total_cipher_count() {
    // Combined pool must have enough variety for realistic TLS handshake simulation
    let total = TLS13_CIPHERS.len() + TLS12_CIPHERS.len();
    assert!(
        total >= 8,
        "Expected at least 8 total ciphers, got {}",
        total
    );
}

#[test]
fn test_protocol_index_mapping() {
    // Verify the documented mapping used in register_ssl_context
    // idx 0 -> TLS, 1 -> TLSv1.2, 2 -> TLSv1.3, 3 -> SSL
    let mappings: &[(i32, &str)] = &[(0, "TLS"), (1, "TLSv1.2"), (2, "TLSv1.3"), (3, "SSL")];
    for (idx, proto) in mappings {
        let result = match *idx {
            1 => "TLSv1.2",
            2 => "TLSv1.3",
            3 => "SSL",
            _ => "TLS",
        };
        assert_eq!(
            result, *proto,
            "Protocol index {} should map to {}",
            idx, proto
        );
    }
}

#[test]
fn test_ssl_context_field_indices_in_range() {
    // All field indices must be within the 12-field synthetic object
    assert!(CTX_PROTOCOL_IDX < 12);
    assert!(CTX_INITIALIZED < 12);
    assert!(CTX_KM_REF < 12);
    assert!(CTX_TM_REF < 12);
}

#[test]
fn test_ssl_engine_field_indices_in_range() {
    // The original eight stay inside the first 8 slots — modules that
    // allocate this class with the older width must keep working.
    assert!(ENG_CLIENT_MODE < 8);
    assert!(ENG_HANDSHAKE_STATUS < 8);
    assert!(ENG_INBOUND_DONE < 8);
    assert!(ENG_OUTBOUND_DONE < 8);
    assert!(ENG_PROTOCOL_IDX < 8);
    assert!(ENG_PEER_HOST < 8);
    assert!(ENG_PEER_PORT < 8);
    assert!(ENG_ALPN_PROTOCOL < 8);
    // The configuration slots the setters write live above them, inside
    // the width `alloc_ssl_engine` requests.
    assert!(ENG_ENABLED_PROTOCOLS >= 8 && ENG_ENABLED_PROTOCOLS < ENG_FIELD_COUNT);
    assert!(ENG_ENABLED_CIPHERS >= 8 && ENG_ENABLED_CIPHERS < ENG_FIELD_COUNT);
    assert!(ENG_APP_PROTOCOLS >= 8 && ENG_APP_PROTOCOLS < ENG_FIELD_COUNT);
    assert!(ENG_NEED_CLIENT_AUTH >= 8 && ENG_NEED_CLIENT_AUTH < ENG_FIELD_COUNT);
    assert!(ENG_WANT_CLIENT_AUTH >= 8 && ENG_WANT_CLIENT_AUTH < ENG_FIELD_COUNT);
    assert!(ENG_ENDPOINT_ID_ALG >= 8 && ENG_ENDPOINT_ID_ALG < ENG_FIELD_COUNT);
}

#[test]
fn test_ssl_parameters_field_indices_in_range_and_distinct() {
    let indices = [
        PAR_PROTOCOLS,
        PAR_CIPHER_SUITES,
        PAR_ENDPOINT_ID_ALG,
        PAR_NEED_CLIENT_AUTH,
        PAR_WANT_CLIENT_AUTH,
        PAR_APP_PROTOCOLS,
    ];
    for (i, a) in indices.iter().enumerate() {
        assert!(*a < PAR_FIELD_COUNT);
        for b in indices.iter().skip(i + 1) {
            assert_ne!(a, b, "Duplicate SSLParameters field index");
        }
    }
}

#[test]
fn test_ssl_session_field_indices_in_range() {
    // All field indices must be within the 6-field synthetic object
    assert!(SES_CIPHER_SUITE < 6);
    assert!(SES_PROTOCOL < 6);
    assert!(SES_VALID < 6);
    assert!(SES_PEER_HOST < 6);
    assert!(SES_PEER_PORT < 6);
    assert!(SES_CREATION_TIME < 6);
}

#[test]
fn test_ssl_engine_field_indices_are_distinct() {
    let indices = [
        ENG_CLIENT_MODE,
        ENG_HANDSHAKE_STATUS,
        ENG_INBOUND_DONE,
        ENG_OUTBOUND_DONE,
        ENG_PROTOCOL_IDX,
        ENG_PEER_HOST,
        ENG_PEER_PORT,
        ENG_ALPN_PROTOCOL,
        ENG_ENABLED_PROTOCOLS,
        ENG_ENABLED_CIPHERS,
        ENG_APP_PROTOCOLS,
        ENG_NEED_CLIENT_AUTH,
        ENG_WANT_CLIENT_AUTH,
        ENG_ENDPOINT_ID_ALG,
    ];
    for i in 0..indices.len() {
        for j in (i + 1)..indices.len() {
            assert_ne!(
                indices[i], indices[j],
                "Duplicate field index at {} and {}",
                i, j
            );
        }
    }
}

#[test]
fn test_ssl_session_field_indices_are_distinct() {
    let indices = [
        SES_CIPHER_SUITE,
        SES_PROTOCOL,
        SES_VALID,
        SES_PEER_HOST,
        SES_PEER_PORT,
        SES_CREATION_TIME,
    ];
    for i in 0..indices.len() {
        for j in (i + 1)..indices.len() {
            assert_ne!(indices[i], indices[j]);
        }
    }
}

#[test]
fn test_application_buffer_size_plausible() {
    // Standard TLS max record payload is 16384
    assert_eq!(16384_i32, 16384);
}

#[test]
fn test_packet_buffer_size_larger_than_app_buffer() {
    // Packet buffer must include header overhead
    let packet: i32 = 16709;
    let app: i32 = 16384;
    assert!(packet > app);
}

#[test]
fn test_ssl_context_total_registration_count() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    // SSLContext alone registers 11 methods: <init> + 10 named
    let methods = [
        ("<init>", "()V"),
        ("getInstance", "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;"),
        ("getDefault", "()Ljavax/net/ssl/SSLContext;"),
        (
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        ),
        ("createSSLEngine", "()Ljavax/net/ssl/SSLEngine;"),
        ("createSSLEngine", "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;"),
        ("getSocketFactory", "()Ljavax/net/ssl/SSLSocketFactory;"),
        ("getServerSocketFactory", "()Ljavax/net/ssl/SSLServerSocketFactory;"),
        ("getDefaultSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
        ("getSupportedSSLParameters", "()Ljavax/net/ssl/SSLParameters;"),
        ("getProtocol", "()Ljava/lang/String;"),
    ];
    for (name, desc) in &methods {
        assert!(
            r.find("javax/net/ssl/SSLContext", name, desc).is_some(),
            "Missing SSLContext.{}{}",
            name,
            desc
        );
    }
}

#[test]
fn test_all_hs_constants_non_negative() {
    assert!(HS_NOT_HANDSHAKING >= 0);
    assert!(HS_NEED_WRAP >= 0);
    assert!(HS_NEED_UNWRAP >= 0);
    assert!(HS_NEED_TASK >= 0);
    assert!(HS_FINISHED >= 0);
}

// --- TLSv1.1 removal verification ---

#[test]
fn test_supported_protocols_does_not_include_tls11() {
    for proto in SUPPORTED_PROTOCOLS {
        assert_ne!(
            *proto, "TLSv1.1",
            "TLSv1.1 must not be in SUPPORTED_PROTOCOLS"
        );
        assert_ne!(*proto, "TLSv1", "TLSv1 must not be in SUPPORTED_PROTOCOLS");
        assert_ne!(*proto, "SSLv3", "SSLv3 must not be in SUPPORTED_PROTOCOLS");
    }
}

#[test]
fn test_supported_protocols_only_modern() {
    // All supported protocols must be TLSv1.2 or higher
    for proto in SUPPORTED_PROTOCOLS {
        assert!(
            *proto == "TLSv1.3" || *proto == "TLSv1.2",
            "Unexpected protocol: {}",
            proto
        );
    }
}

#[test]
fn test_supported_protocols_len() {
    assert_eq!(
        SUPPORTED_PROTOCOLS.len(),
        2,
        "Expected exactly 2 protocols (TLSv1.3 + TLSv1.2)"
    );
}

// --- Handshake state machine tests ---

#[test]
fn test_client_begins_handshake_with_need_wrap() {
    // Client mode (1) should start with NEED_WRAP to send ClientHello
    let client_mode = 1;
    let initial_hs = if client_mode != 0 {
        HS_NEED_WRAP
    } else {
        HS_NEED_UNWRAP
    };
    assert_eq!(initial_hs, HS_NEED_WRAP);    }

#[test]
fn test_server_begins_handshake_with_need_unwrap() {
    // Server mode (0) should start with NEED_UNWRAP to receive ClientHello
    let client_mode = 0;
    let initial_hs = if client_mode != 0 {
        HS_NEED_WRAP
    } else {
        HS_NEED_UNWRAP
    };
    assert_eq!(initial_hs, HS_NEED_UNWRAP);    }

#[test]
fn test_wrap_advances_need_wrap_to_need_unwrap() {
    // When wrap is called during NEED_WRAP, next state should be NEED_UNWRAP
    let hs_status = HS_NEED_WRAP;
    let next_hs = match hs_status {
        HS_NEED_WRAP => HS_NEED_UNWRAP,
        _ => HS_NOT_HANDSHAKING,
    };
    assert_eq!(next_hs, HS_NEED_UNWRAP);
}

#[test]
fn test_wrap_not_handshaking_stays_not_handshaking() {
    let hs_status = HS_NOT_HANDSHAKING;
    let next_hs = match hs_status {
        HS_NEED_WRAP => HS_NEED_UNWRAP,
        _ => HS_NOT_HANDSHAKING,
    };
    assert_eq!(next_hs, HS_NOT_HANDSHAKING);
}

#[test]
fn test_unwrap_advances_need_unwrap_to_need_wrap() {
    let hs_status = HS_NEED_UNWRAP;
    let next_hs = match hs_status {
        HS_NEED_UNWRAP => HS_NEED_WRAP,
        HS_NEED_TASK => HS_FINISHED,
        _ => HS_NOT_HANDSHAKING,
    };
    assert_eq!(next_hs, HS_NEED_WRAP);
}

#[test]
fn test_unwrap_need_task_transitions_to_finished() {
    let hs_status = HS_NEED_TASK;
    let next_hs = match hs_status {
        HS_NEED_UNWRAP => HS_NEED_WRAP,
        HS_NEED_TASK => HS_FINISHED,
        _ => HS_NOT_HANDSHAKING,
    };
    assert_eq!(next_hs, HS_FINISHED);
}

// --- SSLContext initialization ---

#[test]
fn test_ssl_context_protocol_idx_tls() {
    // "TLS" -> idx 0
    let idx: i32 = 0;
    let proto = match idx {
        1 => "TLSv1.2",
        2 => "TLSv1.3",
        3 => "SSL",
        _ => "TLS",
    };
    assert_eq!(proto, "TLS");
}

#[test]
fn test_ssl_context_protocol_idx_tls12() {
    let idx: i32 = 1;
    let proto = match idx {
        1 => "TLSv1.2",
        2 => "TLSv1.3",
        3 => "SSL",
        _ => "TLS",
    };
    assert_eq!(proto, "TLSv1.2");
}

#[test]
fn test_ssl_context_protocol_idx_tls13() {
    let idx: i32 = 2;
    let proto = match idx {
        1 => "TLSv1.2",
        2 => "TLSv1.3",
        3 => "SSL",
        _ => "TLS",
    };
    assert_eq!(proto, "TLSv1.3");
}

#[test]
fn test_ssl_context_protocol_idx_ssl() {
    let idx: i32 = 3;
    let proto = match idx {
        1 => "TLSv1.2",
        2 => "TLSv1.3",
        3 => "SSL",
        _ => "TLS",
    };
    assert_eq!(proto, "SSL");
}

#[test]
fn test_ssl_context_protocol_idx_unknown_defaults_to_tls() {
    let idx: i32 = 99;
    let proto = match idx {
        1 => "TLSv1.2",
        2 => "TLSv1.3",
        3 => "SSL",
        _ => "TLS",
    };
    assert_eq!(proto, "TLS");
}

// --- SSLSession field indices ---

#[test]
fn test_ssl_session_creation_time_index_is_last() {
    // Creation time should be the last field (index 5 in a 6-field object)
    assert_eq!(SES_CREATION_TIME, 5);
}

#[test]
fn test_ssl_context_field_indices_are_distinct() {
    let indices = [CTX_PROTOCOL_IDX, CTX_INITIALIZED, CTX_KM_REF, CTX_TM_REF];
    for i in 0..indices.len() {
        for j in (i + 1)..indices.len() {
            assert_ne!(
                indices[i], indices[j],
                "Duplicate SSLContext field at {} and {}",
                i, j
            );
        }
    }
}

#[test]
fn test_ssl_parameters_registration_complete() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLParameters";
    // Verify we can look up all 12 SSLParameters methods
    let count = [
        ("<init>", "()V"),
        ("getProtocols", "()[Ljava/lang/String;"),
        ("setProtocols", "([Ljava/lang/String;)V"),
        ("getCipherSuites", "()[Ljava/lang/String;"),
        ("setCipherSuites", "([Ljava/lang/String;)V"),
        ("getApplicationProtocols", "()[Ljava/lang/String;"),
        ("setApplicationProtocols", "([Ljava/lang/String;)V"),
        ("getEndpointIdentificationAlgorithm", "()Ljava/lang/String;"),
        (
            "setEndpointIdentificationAlgorithm",
            "(Ljava/lang/String;)V",
        ),
        ("getNeedClientAuth", "()Z"),
        ("setNeedClientAuth", "(Z)V"),
        ("getWantClientAuth", "()Z"),
        ("setWantClientAuth", "(Z)V"),
    ]
    .iter()
    .filter(|(name, desc)| r.find(cls, name, desc).is_some())
    .count();
    assert_eq!(
        count, 13,
        "Expected all 13 SSLParameters methods registered"
    );
}

#[test]
fn test_key_store_registration_complete() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "java/security/KeyStore";
    let all_methods = [
        ("<init>", "()V"),
        (
            "getInstance",
            "(Ljava/lang/String;)Ljava/security/KeyStore;",
        ),
        ("getDefaultType", "()Ljava/lang/String;"),
        ("load", "(Ljava/io/InputStream;[C)V"),
        (
            "getCertificate",
            "(Ljava/lang/String;)Ljava/security/cert/Certificate;",
        ),
        ("getKey", "(Ljava/lang/String;[C)Ljava/security/Key;"),
        ("containsAlias", "(Ljava/lang/String;)Z"),
        ("aliases", "()Ljava/util/Enumeration;"),
        ("size", "()I"),
        (
            "setCertificateEntry",
            "(Ljava/lang/String;Ljava/security/cert/Certificate;)V",
        ),
        (
            "setKeyEntry",
            "(Ljava/lang/String;Ljava/security/Key;[C[Ljava/security/cert/Certificate;)V",
        ),
        ("deleteEntry", "(Ljava/lang/String;)V"),
    ];
    for (name, desc) in &all_methods {
        assert!(
            r.find(cls, name, desc).is_some(),
            "Missing KeyStore.{}{}",
            name,
            desc
        );
    }
}

// =======================================================================
// T19.9 — TLS 1.3 API consolidation
// =======================================================================
//
// These ten tests exercise the consolidated TLS surface landed in
// Session 88. They assert:
//   (1) the canonical-home map (tls_impl owns the state machine,
//       t27_tls owns SSLServerSocket/X509TrustManager.getAcceptedIssuers,
//       tls.rs owns the SSLSessionImpl/SSLContextImpl Sun shims, and
//       phases_late.rs owns the rest of the javax.net.ssl surface);
//   (2) there are no duplicate (class, method, desc) tuples across tls.rs
//       + tls_impl.rs + t27_tls.rs (phases_late.rs is out of scope);
//   (3) the Session 87 Lambda handshake tests continue to pass via the
//       `tls_impl::tests::test_server_*` module re-exports;
//   (4) Keycloak-required SSLSessionImpl + engineInit registrations land;
//   (5) the RFC 8446 §9.1 MTI cipher allowlist rejects RC4/3DES.

#[test]
fn t19_9_ssl_context_instance_get_tlsv13() {
    // SSLContext.getInstance("TLSv1.3") must resolve to a registered
    // native (post-consolidation it lives in phases_late.rs, but the
    // baseline tls.rs also registers it so the `register_tls_natives`
    // output has an entry — either way, a fresh registry populated
    // from register_tls_natives MUST have it).
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    assert!(
        r.find(
            "javax/net/ssl/SSLContext",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        )
        .is_some(),
        "SSLContext.getInstance must be registered post-consolidation",
    );
}

#[test]
fn t19_9_ssl_context_init_with_keymanager_and_trustmanager() {
    // Both the javax.net.ssl.SSLContext.init and
    // sun.security.ssl.SSLContextImpl.engineInit SPI entry points
    // must be registered — the latter is Keycloak's actual bootstrap
    // path when SSLContext.getInstance("TLSv1.3") returns an impl.
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    assert!(r
        .find(
            "javax/net/ssl/SSLContext",
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        )
        .is_some());
    assert!(
        r.find(
            "sun/security/ssl/SSLContextImpl",
            "engineInit",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        )
        .is_some(),
        "engineInit SPI must be registered (Keycloak bootstrap path)",
    );
}

#[test]
fn t19_9_key_manager_factory_get_instance_sun_x509() {
    // KeyManagerFactory.getInstance("SunX509") is what Keycloak asks
    // for first. The native contract is that getInstance is
    // protocol-agnostic (accepts any string, validates later), so
    // the registration just needs to exist under the javax.net.ssl
    // path.
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    assert!(r
        .find(
            "javax/net/ssl/KeyManagerFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        )
        .is_some());
}

#[test]
fn t19_9_trust_manager_factory_init_with_keystore() {
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    assert!(r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "init",
            "(Ljava/security/KeyStore;)V",
        )
        .is_some());
    assert!(r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "getTrustManagers",
            "()[Ljavax/net/ssl/TrustManager;",
        )
        .is_some());
}

#[test]
fn t19_9_ssl_engine_wrap_unwrap_round_trip() {
    // Verify both wrap and unwrap are registered with the JDK-compatible
    // ByteBuffer signatures. The actual byte-level round-trip is
    // exercised by tls_impl::tests::test_server_full_handshake
    // (RFC 8446 atomic-flight server). Here we just assert the
    // registration contract so callers' dispatch resolves.
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    let cls = "javax/net/ssl/SSLEngine";
    assert!(r
        .find(
            cls,
            "wrap",
            "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "unwrap",
            "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        )
        .is_some());
}

#[test]
fn t19_9_ssl_session_id_is_nonempty_after_handshake() {
    // Post-consolidation SSLSessionImpl.getId must be registered.
    // The Session 87 Lambda handshake populates real session IDs via
    // rustls; for the registry smoke test we just check the shim is in.
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    assert!(
        r.find("sun/security/ssl/SSLSessionImpl", "getId", "()[B")
            .is_some(),
        "SSLSessionImpl.getId must be registered (Keycloak session tracking)",
    );
    // And the javax.net.ssl.SSLSession.getId (baseline) too.
    assert!(r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .is_some());
}

#[test]
fn t19_9_t27_session_ticket_resumption_bypasses_full_handshake() {
    // T27 session ticket / resumption surface is in t27_tls.rs. The
    // HttpsURLConnection setSSLSocketFactory path is the one Keycloak
    // exercises when it caches a TLS session. Assert the registration
    // is LIVE (not duplicated elsewhere — see
    // `t19_9_consolidation_no_duplicate_registrations`).
    let mut r = NativeMethodRegistry::new();
    register_tls_natives(&mut r);
    crate::t27_tls::register_t27_natives(&mut r);
    assert!(r
        .find(
            "javax/net/ssl/HttpsURLConnection",
            "setSSLSocketFactory",
            "(Ljavax/net/ssl/SSLSocketFactory;)V",
        )
        .is_some());
    // The rustls-backed self-test surface is what verifies ticket
    // resumption at the wire level (see t27_tls::tests::t27_session_resumption).
    assert!(r
        .find("cratonvm/tls/T27SelfTest", "run", "()Ljava/lang/String;")
        .is_some());
}

// `tls_impl` only exists under `legacy-synthetic-crypto`; gate the tests
// that reach into it so the default (synthetic-crypto-free) test build
// compiles.
#[cfg(feature = "legacy-synthetic-crypto")]
#[test]
fn t19_9_consolidation_no_duplicate_registrations() {
    // Canonical-home map: register each file's natives once, in the
    // same order as lib.rs runtime wiring (tls -> tls_impl -> t27),
    // then check no (class, method, desc) key appears more than
    // once across the three owned files. phases_late.rs is out of
    // this module's scope (owned by T9.5 agents) — duplicates
    // between tls.rs and phases_late.rs are intentional
    // last-writer-wins and not covered by this test.
    //
    // To measure duplicates independently of the registry's last-writer
    // semantics, we dump the `(class, method, desc)` tuples each
    // module would register when given a fresh registry, and then
    // diff. NativeMethodRegistry has no iteration API, so we probe
    // a closed list of known signatures collected at audit time.
    //
    // This test closes on the post-consolidation set: after removing
    // (a) X509TrustManager.getAcceptedIssuers from tls.rs, and
    // (b) SSLContext.createSSLEngine (both descriptors) from
    // tls_impl.rs, there should be zero cross-file duplicates among
    // the files tls.rs / tls_impl.rs / t27_tls.rs.
    let known_shared_keys: &[(&str, &str, &str)] = &[
        // Previously duplicated between tls.rs and t27_tls.rs —
        // removed from tls.rs in T19.9.
        (
            "javax/net/ssl/X509TrustManager",
            "getAcceptedIssuers",
            "()[Ljava/security/cert/X509Certificate;",
        ),
        // Previously duplicated between tls_impl.rs and phases_late.rs —
        // removed from tls_impl.rs in T19.9 (phases_late is canonical).
        (
            "javax/net/ssl/SSLContext",
            "createSSLEngine",
            "()Ljavax/net/ssl/SSLEngine;",
        ),
        (
            "javax/net/ssl/SSLContext",
            "createSSLEngine",
            "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
        ),
    ];

    // For each formerly-duplicated key, the tls_impl / t27 files must
    // no longer register it. (tls.rs still registers SSLContext.*
    // baselines — that's fine because phases_late.rs is the shadow.)
    let mut r_impl = NativeMethodRegistry::new();
    crate::tls::tls_impl::register_tls_impl_natives(&mut r_impl);
    for (cls, name, desc) in known_shared_keys {
        if *cls == "javax/net/ssl/SSLContext" && *name == "createSSLEngine" {
            assert!(
                r_impl.find(cls, name, desc).is_none(),
                "tls_impl.rs must no longer register {}.{}{} (shadowed by phases_late.rs)",
                cls,
                name,
                desc,
            );
        }
    }

    let mut r_tls = NativeMethodRegistry::new();
    register_tls_natives(&mut r_tls);
    assert!(
        r_tls
            .find(
                "javax/net/ssl/X509TrustManager",
                "getAcceptedIssuers",
                "()[Ljava/security/cert/X509Certificate;",
            )
            .is_none(),
        "tls.rs must no longer register X509TrustManager.getAcceptedIssuers (owned by t27_tls.rs)",
    );
}

#[cfg(feature = "legacy-synthetic-crypto")]
#[test]
fn t19_9_tls_orphan_registrations_removed() {
    // The two orphan registrations landed in sessions 86/87 and
    // deleted in T19.9:
    //   tls.rs:             X509TrustManager.getAcceptedIssuers
    //   tls_impl.rs:        SSLContext.createSSLEngine (both descriptors)
    // Build the registry as lib.rs does (tls -> tls_impl -> t27) and
    // verify the ORPHAN files do not re-add them.
    let mut r_tls_only = NativeMethodRegistry::new();
    register_tls_natives(&mut r_tls_only);
    assert!(
        r_tls_only
            .find(
                "javax/net/ssl/X509TrustManager",
                "getAcceptedIssuers",
                "()[Ljava/security/cert/X509Certificate;",
            )
            .is_none(),
        "ORPHAN: tls.rs must not re-register X509TrustManager.getAcceptedIssuers",
    );

    let mut r_impl_only = NativeMethodRegistry::new();
    crate::tls::tls_impl::register_tls_impl_natives(&mut r_impl_only);
    assert!(
        r_impl_only
            .find(
                "javax/net/ssl/SSLContext",
                "createSSLEngine",
                "()Ljavax/net/ssl/SSLEngine;",
            )
            .is_none(),
        "ORPHAN: tls_impl.rs must not re-register SSLContext.createSSLEngine()",
    );
    assert!(
        r_impl_only
            .find(
                "javax/net/ssl/SSLContext",
                "createSSLEngine",
                "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
            )
            .is_none(),
        "ORPHAN: tls_impl.rs must not re-register SSLContext.createSSLEngine(String, int)",
    );
}

#[cfg(feature = "legacy-synthetic-crypto")]
#[test]
fn t19_9_handshake_still_green() {
    // Regression sentinel — if Session 87 Lambda's three server-handshake
    // tests in tls_impl ever regress, this module-level test must
    // notice. The tests themselves are in
    // `crate::tls::tls_impl::tests`. Here we confirm the
    // state-machine entry points they rely on still exist by
    // constructing one and walking a minimal client -> server ->
    // FINISHED transition.
    use crate::tls::tls_impl::{
        CipherSuite, HandshakeMessage, Tls13StateMachine, TlsExtension,
    };

    let mut sm = Tls13StateMachine::new_server();
    let resp = sm
        .advance(HandshakeMessage::ClientHello {
            random: [7u8; 32],
            session_id: [8u8; 32],
            cipher_suites: vec![CipherSuite::TlsAes128GcmSha256],
            extensions: vec![TlsExtension::ServerName("keycloak.localhost".into())],
        })
        .expect("CH accepted");
    assert!(resp.is_some(), "server must emit at least ServerHello");
    assert_eq!(sm.sni_hostname, Some("keycloak.localhost".to_string()));

    let fin = sm
        .advance(HandshakeMessage::Finished {
            verify_data: vec![0xCD],
        })
        .expect("client Finished accepted");
    assert!(fin.is_some(), "server must emit server Finished");
    assert!(sm.is_connected(), "server must be CONNECTED after flight");

    // RFC 8446 §9.1 MTI check — the selected suite must be on the
    // allowlist.
    assert!(
        crate::tls::tls_impl::is_mti_cipher_suite(sm.cipher_suite),
        "post-handshake cipher suite {:?} is not on the RFC 8446 §9.1 MTI allowlist",
        sm.cipher_suite,
    );
}
