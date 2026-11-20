use super::*;
use cratonvm_native_api::NativeMethodRegistry;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

fn build_registry() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    register_p68_ssl(&mut r);
    r
}

/// Like `build_registry`, but also wires the JCA provider chain
/// (`crate::jca::register_jca_natives`, idempotent per its own doc
/// comment) so `provider_chain::find_service_provider` resolves the
/// built-in SunJSSE `KeyManagerFactory`/`TrustManagerFactory` services —
/// real VM boot always registers both; a bare `register_p68_ssl` alone
/// leaves the provider-chain seed data (`seed_sunjsse_services`) unrun,
/// so `getInstance("SunX509")`/`getInstance("PKIX")` would wrongly throw
/// `NoSuchAlgorithmException` in a test using plain `build_registry()`.
fn build_registry_with_jca() -> NativeMethodRegistry {
    let mut r = NativeMethodRegistry::new();
    crate::jca::register_jca_natives(&mut r);
    register_p68_ssl(&mut r);
    r
}

fn cb_addr(r: &NativeMethodRegistry, cls: &str, name: &str, desc: &str) -> Option<usize> {
    r.find(cls, name, desc).map(|cb| cb as usize)
}

/// **Which of this file's ten `javax/net/ssl/SSLSession` registrations are
/// alive, stated as an assertion instead of as a comment.**
///
/// G7. This file registers ten triples on `javax/net/ssl/SSLSession`.
/// `t27_tls::register_ssl_session_real` registers seventeen, and it runs
/// LATER on both real-JDK paths — `phases_late.rs:6670` `register_p68_ssl`
/// then `:6674` `register_t27_natives`, and `lib.rs:18665` then `:18726`.
/// Registration is last-write-wins with no unregister API, so seven of this
/// file's ten are dead in the mode `--real-jdk` runs, and three are live:
///
/// ```text
///   DEAD (t27_tls wins)   getProtocol  getCipherSuite  isValid  getId
///                         getPeerCertificates  getCreationTime
///                         getLastAccessedTime
///   LIVE (this file owns) getPeerPrincipal  getLocalCertificates
///                         getLocalPrincipal
/// ```
///
/// This is not hypothetical bookkeeping. E12-1 landed eight edits in this
/// file and E22-1's `--dump-native-registry` dump then found that three of
/// them (`getProtocol`, `getCipherSuite`, `getId`) had gone into bodies
/// that never run — a green build that proved nothing, which is
/// HANDOFF-20260814 §5's named trap. A future lane reading these ten
/// registrations has no way to tell the two groups apart by looking at
/// them, so the split is asserted here: land a session fix in one of the
/// seven and this test still passes, but the fix is inert; MOVE one of the
/// three into `t27_tls` and this test fails and names it.
///
/// SOURCE-VERIFIED (the two `lib.rs`/`phases_late.rs` call orders were
/// read, and the callback identity below is compared directly). NOT
/// verified against a `--dump-native-registry` dump — no binary carrying
/// this change exists yet.
#[test]
fn seven_of_this_files_ssl_session_doors_are_dead_and_three_are_live() {
    let mut p68_only = NativeMethodRegistry::new();
    register_p68_ssl(&mut p68_only);

    // The real-JDK order, as `phases_late::register_phase68_natives` runs
    // it: this file first, `t27_tls` second.
    let mut boot = NativeMethodRegistry::new();
    register_p68_ssl(&mut boot);
    crate::t27_tls::register_sslengine_real(&mut boot);

    let cls = "javax/net/ssl/SSLSession";

    for (name, desc) in [
        ("getProtocol", "()Ljava/lang/String;"),
        ("getCipherSuite", "()Ljava/lang/String;"),
        ("isValid", "()Z"),
        ("getId", "()[B"),
        ("getPeerCertificates", "()[Ljava/security/cert/Certificate;"),
        ("getCreationTime", "()J"),
        ("getLastAccessedTime", "()J"),
    ] {
        let mine = cb_addr(&p68_only, cls, name, desc)
            .unwrap_or_else(|| panic!("register_p68_ssl must register {cls}.{name}{desc}"));
        let winner = cb_addr(&boot, cls, name, desc)
            .unwrap_or_else(|| panic!("{cls}.{name}{desc} must be registered after both"));
        assert_ne!(
            winner, mine,
            "{cls}.{name}{desc}: this file's body is DEAD in real-JDK mode \
             — `t27_tls::register_ssl_session_real` runs later and owns the \
             slot. If this now passes with the two equal, t27_tls has \
             dropped its registration and every measured contract it \
             carries for this door (E12/E31/E42/F18/G7) has silently \
             reverted to this file's older body."
        );
    }

    for (name, desc) in [
        ("getPeerPrincipal", "()Ljava/security/Principal;"),
        (
            "getLocalCertificates",
            "()[Ljava/security/cert/Certificate;",
        ),
        ("getLocalPrincipal", "()Ljava/security/Principal;"),
    ] {
        let mine = cb_addr(&p68_only, cls, name, desc)
            .unwrap_or_else(|| panic!("register_p68_ssl must register {cls}.{name}{desc}"));
        let winner = cb_addr(&boot, cls, name, desc)
            .unwrap_or_else(|| panic!("{cls}.{name}{desc} must be registered after both"));
        assert_eq!(
            winner, mine,
            "{cls}.{name}{desc}: this file's body is the LIVE one — \
             t27_tls deliberately does not re-register these three. If \
             t27_tls has started registering it, this file's body is now \
             dead and any fix landed here (the peer/local principal \
             derivation, the `in_client_trust_check` gate) stopped running."
        );
    }
}

#[test]
fn ssl_context_getinstance_and_init_are_registered() {
    let r = build_registry();
    assert!(r
        .find(
            "javax/net/ssl/SSLContext",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/SSLContext;",
        )
        .is_some());
    assert!(r
        .find(
            "javax/net/ssl/SSLContext",
            "init",
            "([Ljavax/net/ssl/KeyManager;[Ljavax/net/ssl/TrustManager;Ljava/security/SecureRandom;)V",
        )
        .is_some());
}

#[test]
fn ssl_socket_factory_create_socket_is_registered() {
    let r = build_registry();
    assert!(r
        .find(
            "javax/net/ssl/SSLSocketFactory",
            "createSocket",
            "(Ljava/lang/String;I)Ljava/net/Socket;",
        )
        .is_some());
    assert!(r
        .find(
            "javax/net/ssl/SSLSocketFactory",
            "createSocket",
            "(Ljava/net/Socket;Ljava/lang/String;IZ)Ljava/net/Socket;",
        )
        .is_some());
}

#[test]
fn ssl_session_get_peer_certificates_is_registered() {
    let r = build_registry();
    assert!(r
        .find(
            "javax/net/ssl/SSLSession",
            "getPeerCertificates",
            "()[Ljava/security/cert/Certificate;",
        )
        .is_some());
}

#[test]
fn trust_manager_factory_default_returns_concrete_x509_impl() {
    use crate::test_utils::MockNativeContext;

    let r = build_registry_with_jca();
    let mut ctx = MockNativeContext::new();
    let get_instance = r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        )
        .unwrap();
    // FIX (rabbitautoconfigurationtests-cglib-enhance-hang residual):
    // `getInstance` now validates the algorithm against the provider
    // chain (real JDK throws `NoSuchAlgorithmException` for an unknown
    // one), so a null/empty algorithm no longer silently succeeds.
    // "PKIX" is a real, registered `TrustManagerFactory` algorithm — the
    // test only cares that SOME factory is returned, not which
    // algorithm, so this is a like-for-like substitution.
    let algo = ctx.create_string("PKIX");
    let factory = match get_instance(&mut ctx, &[Value::Object(Some(algo))]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        other => panic!("getInstance should return a factory, got {other:?}"),
    };
    // `getTrustManagers()` now refuses an un-`init`ed factory, exactly as
    // `TrustManagerFactoryImpl` does — so this test has to init one
    // before it can ask what the manager's CLASS is, which is the only
    // thing it was ever about. `init((KeyStore) null)` is the
    // platform-trust-store form and needs no fixture.
    let init = r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "init",
            "(Ljava/security/KeyStore;)V",
        )
        .unwrap();
    init(
        &mut ctx,
        &[Value::Object(Some(factory)), Value::Object(None)],
    )
    .expect("init((KeyStore) null) should succeed");
    let get_tms = r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "getTrustManagers",
            "()[Ljavax/net/ssl/TrustManager;",
        )
        .unwrap();
    let arr = match get_tms(&mut ctx, &[Value::Object(Some(factory))]) {
        Ok(Some(Value::Object(Some(a)))) => a,
        other => panic!("getTrustManagers should return an array, got {other:?}"),
    };
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
        "default TrustManagerFactory must not vend the abstract X509TrustManager interface"
    );
}

// FIX (rabbitautoconfigurationtests-cglib-enhance-hang residual):
// `KeyManagerFactory`/`TrustManagerFactory.getInstance` must honour the
// real JCA `getInstance` contract — throw for an algorithm no provider
// claims, succeed for one that's actually registered. Regression tests
// for `RabbitAutoConfigurationTests.
// enableSslWith{Invalid,}{KeyStore,TrustStore}AlgorithmShouldFail`.
#[test]
fn key_manager_factory_get_instance_accepts_known_algorithm() {
    use crate::test_utils::MockNativeContext;
    let r = build_registry_with_jca();
    let mut ctx = MockNativeContext::new();
    let get_instance = r
        .find(
            "javax/net/ssl/KeyManagerFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        )
        .unwrap();
    let algo = ctx.create_string("SunX509");
    match get_instance(&mut ctx, &[Value::Object(Some(algo))]) {
        Ok(Some(Value::Object(Some(_)))) => {}
        other => panic!("getInstance(\"SunX509\") should succeed, got {other:?}"),
    }
}

#[test]
fn key_manager_factory_get_instance_rejects_unknown_algorithm() {
    use crate::test_utils::MockNativeContext;
    let r = build_registry();
    let mut ctx = MockNativeContext::new();
    let get_instance = r
        .find(
            "javax/net/ssl/KeyManagerFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/KeyManagerFactory;",
        )
        .unwrap();
    let algo = ctx.create_string("test-invalid-algo");
    match get_instance(&mut ctx, &[Value::Object(Some(algo))]) {
        Err(MethodCallFailed::ExceptionThrown(_))
        | Err(MethodCallFailed::InternalError(_))
        | Ok(None) => {}
        Ok(Some(other)) => {
            panic!("getInstance(\"test-invalid-algo\") must not succeed, got {other:?}")
        }
    }
}

#[test]
fn trust_manager_factory_get_instance_accepts_known_algorithm() {
    use crate::test_utils::MockNativeContext;
    let r = build_registry_with_jca();
    let mut ctx = MockNativeContext::new();
    let get_instance = r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        )
        .unwrap();
    let algo = ctx.create_string("PKIX");
    match get_instance(&mut ctx, &[Value::Object(Some(algo))]) {
        Ok(Some(Value::Object(Some(_)))) => {}
        other => panic!("getInstance(\"PKIX\") should succeed, got {other:?}"),
    }
}

#[test]
fn trust_manager_factory_get_instance_rejects_unknown_algorithm() {
    use crate::test_utils::MockNativeContext;
    let r = build_registry();
    let mut ctx = MockNativeContext::new();
    let get_instance = r
        .find(
            "javax/net/ssl/TrustManagerFactory",
            "getInstance",
            "(Ljava/lang/String;)Ljavax/net/ssl/TrustManagerFactory;",
        )
        .unwrap();
    let algo = ctx.create_string("test-invalid-algo");
    match get_instance(&mut ctx, &[Value::Object(Some(algo))]) {
        Err(MethodCallFailed::ExceptionThrown(_))
        | Err(MethodCallFailed::InternalError(_))
        | Ok(None) => {}
        Ok(Some(other)) => {
            panic!("getInstance(\"test-invalid-algo\") must not succeed, got {other:?}")
        }
    }
}

#[test]
fn ssl_field_layout_constants_are_distinct() {
    // Sanity-check the NEW-13 field layout so a later edit that inserts
    // a new field cannot silently duplicate an index.
    let ctx_ids = [
        NEW13_CTX_PROTOCOL,
        NEW13_CTX_INIT,
        NEW13_CTX_KM,
        NEW13_CTX_TM,
        NEW13_CTX_RANDOM,
    ];
    for i in 0..ctx_ids.len() {
        for j in (i + 1)..ctx_ids.len() {
            assert_ne!(ctx_ids[i], ctx_ids[j], "duplicate SSLContext field index");
        }
        assert!(ctx_ids[i] < NEW13_SSL_CTX_FIELDS);
    }
    let sess_ids = [
        NEW13_SESS_PROTO,
        NEW13_SESS_CIPHER,
        NEW13_SESS_TLSID,
        NEW13_SESS_ATTRS,
    ];
    for i in 0..sess_ids.len() {
        for j in (i + 1)..sess_ids.len() {
            assert_ne!(sess_ids[i], sess_ids[j], "duplicate SSLSession field index");
        }
        assert!(sess_ids[i] < NEW13_SSL_SESS_FIELDS);
    }
}

// -----------------------------------------------------------------------
// E42 — the widened session shape, and the co-requisite it must not
// outlive
// -----------------------------------------------------------------------

/// **The co-requisite, mechanised.** Widening `NEW13_SSL_SESS_FIELDS` from
/// 3 to 4 gives this shape a dedicated attribute slot — and moves it out of
/// `t27_tls::session_has_negotiated`'s `3 =>` arm, which reads the stream
/// id, into an arm that must read it too. If that arm is ever the
/// `_ => true` one, the null session becomes VALID again and `getId()` goes
/// back to 32 fabricated bytes: the exact defect the E12 and E22 lanes
/// removed, re-created by a widening meant to fix a different one.
///
/// A comment saying "land these together" cannot fail a build. This can.
#[test]
fn the_widened_null_session_is_still_not_negotiated() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(ClassId::new(0), NEW13_SSL_SESS_FIELDS);
    ctx.set_field(sess, NEW13_SESS_TLSID, Value::Int(-1));
    ctx.set_field(sess, NEW13_SESS_ATTRS, Value::Object(None));
    assert!(
        !crate::t27_tls::session_has_negotiated(&ctx, sess),
        "a {}-field session carrying tls_id = -1 has negotiated NOTHING. \
         `t27_tls::session_has_negotiated`'s arm for this width must read \
         slot {} (>= 0), not answer `true` unconditionally — see \
         NEW13_SSL_SESS_FIELDS's CAVEAT. HotSpot 25.0.3+9-LTS, \
         unconnected SSLSocket: isValid() = false, getId() = byte[0].",
        NEW13_SSL_SESS_FIELDS,
        NEW13_SESS_TLSID
    );
}

/// MUTATION CHECK for the test above: without it,
/// `session_has_negotiated` could answer `false` for every shape and the
/// first test would still pass — measuring one branch and calling it
/// coverage, the shape this directory keeps recording.
#[test]
fn the_widened_session_still_reports_a_real_stream_as_negotiated() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(ClassId::new(0), NEW13_SSL_SESS_FIELDS);
    ctx.set_field(sess, NEW13_SESS_TLSID, Value::Int(7));
    ctx.set_field(sess, NEW13_SESS_ATTRS, Value::Object(None));
    assert!(
        crate::t27_tls::session_has_negotiated(&ctx, sess),
        "a session that recorded stream id 7 DID negotiate"
    );
}

/// The widening exists so the attribute API has somewhere to write that is
/// not the stream id. This pins the slot the two files have to agree on:
/// `t27_tls::sslsess_attrs_slot` answers `Some(3)` for width 4, and slot 3
/// is `NEW13_SESS_ATTRS`. If they ever part company, a `putValue` lands on
/// `NEW13_SESS_TLSID` again.
#[test]
fn the_attribute_slot_is_the_last_one_and_is_not_the_stream_id() {
    assert_eq!(
        NEW13_SESS_ATTRS,
        NEW13_SSL_SESS_FIELDS - 1,
        "`t27_tls::sslsess_attrs_slot` resolves this shape's attribute slot \
         from its WIDTH; the dedicated slot must be the last one"
    );
    assert_ne!(
        NEW13_SESS_ATTRS, NEW13_SESS_TLSID,
        "the whole point of the widening: `putValue` must not be able to \
         overwrite the stream id, which is also the negotiation signal"
    );
}

/// G44 — **the width is load-bearing in `t27_tls`, and the two slot maps
/// must agree.** This is the enforcement behind the "DO NOT WIDEN" block on
/// [`NEW13_SSL_SESS_FIELDS`].
///
/// `t27_tls::session_proto_slot`/`session_cipher_slot` derive the protocol
/// and cipher slots FROM THE WIDTH, and they SWAP at `>= 6`. So a lane that
/// widens this shape — the obvious move for `RSslLiveSession`'s
/// `client.peerHost`/`client.peerPort` rows, since `getPeerHost` reads slot
/// 3 on shapes of width `>= 6` — would silently make every minter of this
/// shape report its protocol as its cipher suite and vice versa. Asserted
/// against the live functions rather than against the number 4, so the test
/// keeps meaning what it says if the threshold in `t27_tls` moves instead.
#[test]
fn this_shapes_slot_map_is_the_one_t27_derives_from_its_width() {
    assert_eq!(
        crate::t27_tls::session_proto_slot(NEW13_SSL_SESS_FIELDS),
        Some(NEW13_SESS_PROTO),
        "widening this shape past t27_tls's `>= 6` boundary swaps the protocol and \
         cipher slots. See the G44 block on NEW13_SSL_SESS_FIELDS: the peer host and \
         port belong in a session-object-keyed side table, not in a wider object."
    );
    assert_eq!(
        crate::t27_tls::session_cipher_slot(NEW13_SSL_SESS_FIELDS),
        Some(NEW13_SESS_CIPHER),
        "same boundary, other half of the pair"
    );
}

/// E42 — the two spellings of the enabled-protocol pair, in HotSpot's
/// order. `RSslNullSession` asserts the exact string `[TLSv1.3, TLSv1.2]`,
/// so the order is not cosmetic.
#[test]
fn the_enabled_protocol_pair_is_most_preferred_first() {
    assert_eq!(
        JSSE_ENABLED_PROTOCOLS,
        ["TLSv1.3", "TLSv1.2"],
        "HotSpot 25.0.3+9-LTS, fresh SSLEngine and unconnected SSLSocket \
         alike: getEnabledProtocols() = [TLSv1.3, TLSv1.2]"
    );
}

/// E42 — enabled == supported, measured, so the three doors that answer a
/// suite list must answer the SAME list. The three inline copies this
/// replaced had drifted to 13, 13 and 7 entries.
#[test]
fn every_suite_list_door_answers_the_one_supported_list() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    let r = build_registry();
    let mut ctx = MockNativeContext::new();
    let expected = crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES.len();
    for (cls, name) in [
        ("javax/net/ssl/SSLSocketFactory", "getDefaultCipherSuites"),
        ("javax/net/ssl/SSLSocketFactory", "getSupportedCipherSuites"),
        ("javax/net/ssl/SSLSocket", "getSupportedCipherSuites"),
        ("javax/net/ssl/SSLSocket", "getEnabledCipherSuites"),
    ] {
        let f = r
            .find(cls, name, "()[Ljava/lang/String;")
            .unwrap_or_else(|| panic!("{cls}.{name} registered"));
        match f(&mut ctx, &[Value::Object(None)]) {
            Ok(Some(Value::Object(Some(a)))) => assert_eq!(
                ctx.array_length(a),
                expected,
                "{cls}.{name} must answer the one supported list — HotSpot \
                 25.0.3+9-LTS measures default == supported == enabled, \
                 element-wise (scratchpad/e42/E42FactoryDefaults.java)"
            ),
            other => panic!("{cls}.{name} must return a String[], got {other:?}"),
        }
    }
}

// -----------------------------------------------------------------------
// E12 — the "nothing was negotiated" session
// -----------------------------------------------------------------------
//
// These exist because of the lesson E3-1 recorded one lane earlier: a
// family of fallbacks named only in a document sat unpatched for a day,
// because **a document cannot fail a build**. The behavioural half is
// `regression-suite/src/RSslNullSession.java`; this half needs no VM.

/// The two spellings, pinned to the HotSpot transcript that produced them.
///
/// If someone "tidies" `SSL_NULL_WITH_NULL_NULL` into `NULL_NULL` or
/// `"NONE"` into `"UNKNOWN"`, this fails with the transcript in the
/// message rather than with a silent behaviour change nobody re-measures.
#[test]
fn the_null_session_sentinels_are_the_measured_jsse_spellings() {
    assert_eq!(
        JSSE_NULL_CIPHER_SUITE, "SSL_NULL_WITH_NULL_NULL",
        "HotSpot 25.0.3+9-LTS, unconnected SSLSocket: \
         getSession().getCipherSuite() = SSL_NULL_WITH_NULL_NULL"
    );
    assert_eq!(
        JSSE_NULL_PROTOCOL, "NONE",
        "HotSpot 25.0.3+9-LTS, unconnected SSLSocket: \
         getSession().getProtocol() = NONE"
    );
    // The property that makes them safe to RETURN, stated where it can be
    // broken: neither may ever appear in the suite list this VM claims to
    // support, or the sentinel stops being distinguishable from a real
    // negotiation and the whole argument for returning it collapses.
    assert!(
        !crate::t27_tls::SUPPORTED_CIPHER_SUITE_NAMES.contains(&JSSE_NULL_CIPHER_SUITE),
        "the null-session sentinel must never be an offerable suite"
    );
}

/// `getProtocol`/`getCipherSuite`/`getId`/`isValid` on a session with no
/// backing stream must answer JSSE's null-session values.
#[test]
fn null_session_accessors_answer_the_sentinels_not_a_null_string() {
    // `NativeHeapAccess` explicitly: `ctx` here is the CONCRETE
    // `MockNativeContext`, not a `dyn NativeContext`, so supertrait
    // methods (`alloc_object`, `set_field`, `read_string`,
    // `array_length`) need their own trait in scope. Same reason the
    // sibling test module at `phases_late.rs:10067` says so out loud.
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    let r = build_registry();
    let mut ctx = MockNativeContext::new();
    // A 3-field session whose slots were never populated — the shape
    // `new13_alloc_null_ssl_session` produces, and also what a dropped
    // layout-guard write leaves behind.
    let sess = ctx.alloc_object(ClassId::new(0), NEW13_SSL_SESS_FIELDS);
    ctx.set_field(sess, NEW13_SESS_TLSID, Value::Int(-1));
    let this = &[Value::Object(Some(sess))];

    let proto = r
        .find(
            "javax/net/ssl/SSLSession",
            "getProtocol",
            "()Ljava/lang/String;",
        )
        .expect("getProtocol registered");
    match proto(&mut ctx, this) {
        Ok(Some(Value::Object(Some(s)))) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some(JSSE_NULL_PROTOCOL));
        }
        other => panic!("getProtocol must never return a null String, got {other:?}"),
    }

    let cipher = r
        .find(
            "javax/net/ssl/SSLSession",
            "getCipherSuite",
            "()Ljava/lang/String;",
        )
        .expect("getCipherSuite registered");
    match cipher(&mut ctx, this) {
        Ok(Some(Value::Object(Some(s)))) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some(JSSE_NULL_CIPHER_SUITE));
        }
        other => panic!("getCipherSuite must never return a null String, got {other:?}"),
    }

    // JSSE answers an EMPTY id, not 32 plausible bytes. Tomcat's
    // `JSSESupport.getSessionId` tests exactly `length == 0`.
    let get_id = r
        .find("javax/net/ssl/SSLSession", "getId", "()[B")
        .expect("getId registered");
    match get_id(&mut ctx, this) {
        Ok(Some(Value::Object(Some(a)))) => {
            assert_eq!(
                ctx.array_length(a),
                0,
                "a session with no id must answer byte[0]"
            );
        }
        other => panic!("getId must return an array, got {other:?}"),
    }

    let is_valid = r
        .find("javax/net/ssl/SSLSession", "isValid", "()Z")
        .expect("isValid registered");
    assert_eq!(is_valid(&mut ctx, this).unwrap(), Some(Value::Int(0)));
}

/// MUTATION CHECK for the test above. Without this, both accessors could
/// be `Ok(sentinel)` unconditionally and the previous test would still
/// pass — it would be measuring the sentinel branch and nothing else.
#[test]
fn a_populated_session_is_not_overwritten_by_the_sentinel() {
    use crate::test_utils::MockNativeContext;
    use cratonvm_native_api::NativeHeapAccess;
    let r = build_registry();
    let mut ctx = MockNativeContext::new();
    let sess = ctx.alloc_object(ClassId::new(0), NEW13_SSL_SESS_FIELDS);
    let p = ctx.create_string("TLSv1.3");
    let c = ctx.create_string("TLS_AES_256_GCM_SHA384");
    ctx.set_field(sess, NEW13_SESS_PROTO, Value::Object(Some(p)));
    ctx.set_field(sess, NEW13_SESS_CIPHER, Value::Object(Some(c)));
    ctx.set_field(sess, NEW13_SESS_TLSID, Value::Int(-1));
    let this = &[Value::Object(Some(sess))];

    let proto = r
        .find(
            "javax/net/ssl/SSLSession",
            "getProtocol",
            "()Ljava/lang/String;",
        )
        .unwrap();
    match proto(&mut ctx, this) {
        Ok(Some(Value::Object(Some(s)))) => {
            assert_eq!(ctx.read_string(s).as_deref(), Some("TLSv1.3"))
        }
        other => panic!("a real negotiated protocol must pass through, got {other:?}"),
    }
    let cipher = r
        .find(
            "javax/net/ssl/SSLSession",
            "getCipherSuite",
            "()Ljava/lang/String;",
        )
        .unwrap();
    match cipher(&mut ctx, this) {
        Ok(Some(Value::Object(Some(s)))) => {
            assert_eq!(
                ctx.read_string(s).as_deref(),
                Some("TLS_AES_256_GCM_SHA384")
            )
        }
        other => panic!("a real negotiated suite must pass through, got {other:?}"),
    }
}

#[test]
fn s2_tls_helpers_reject_unknown_id() {
    // A bogus id must not panic and must return errors / None so the
    // Java-side path surfaces a proper IOException instead of UB.
    assert!(crate::servlet::s2_tls_session_info(999_999).is_none());
    assert!(crate::servlet::s2_tls_peer_cert_chain_der(999_999).is_none());
    let mut buf = [0u8; 4];
    assert!(crate::servlet::s2_tls_read(999_999, &mut buf).is_err());
    assert!(crate::servlet::s2_tls_write(999_999, &[1, 2, 3]).is_err());
    // Idempotent close: unknown id is a no-op rather than an error.
    assert!(crate::servlet::s2_tls_close(999_999).is_ok());
}

#[test]
fn new13_build_connector_succeeds_on_default_platform() {
    // NEW-13.2 DoD: the default connector build (no custom KM/TM) must
    // succeed on every platform supported by native-tls, otherwise
    // SSLContext.init would fail even for the trivial null-TM path.
    let c = new13_build_connector(&[], false, None, None);
    assert!(c.is_ok(), "connector build failed: {:?}", c.err());
}

#[test]
fn new13_build_connector_tolerates_unparseable_extra_root() {
    // FIX (es-restclient-https): a garbage "DER" (e.g. from a corrupt or
    // unexpected TrustManager) must be skipped rather than failing the
    // whole connector build — `new13_build_connector` logs and continues.
    let garbage = vec![0xFFu8, 0x00, 0x01, 0x02];
    let c = new13_build_connector(&[garbage], false, None, None);
    assert!(
        c.is_ok(),
        "connector build must tolerate an unparseable extra root: {:?}",
        c.err()
    );
}

// -----------------------------------------------------------------------
// W4-3 — the `javax.crypto.Mac` engine tells the truth about what it does
//
// These are pure-function tests on purpose: they need no `NativeContext`,
// so they cannot be voided by the mock/production `get_field_by_name`
// divergence that `docs/architecture/natives-over-real-jdk-classes.md` §4
// catalogues.
// -----------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Every `Mac` service the REAL `SunJCE` provider publishes, split into the
/// ones this engine computes and the ones it refuses — and both halves are
/// ratcheted.
///
/// ## What this test used to be, and why that was the E25 defect shape
///
/// It held ONE list of 12 names, described as "the names `mac_compute_hmac`
/// implements are exactly the `SunJCE` `Mac` services `jca::provider_chain`
/// seeds", and asserted each was supported. Both sides of that "exactly"
/// were internal to this tree: the 12 were transcribed from
/// `provider_chain.rs:1371`'s seed, which was itself written alongside the
/// `mac_compute_hmac` arms. So the population was the answer, and the only
/// input that could turn it red was deleting an arm from `mac_compute_hmac`.
///
/// In particular it could not go red for the direction
/// `provider_chain.rs:1376` names as "the quiet half of this defect species,
/// because nothing asks for a name nobody publishes": an arm added to
/// `mac_compute_hmac` for a name the seed does not carry. The sibling
/// `provider_chain::every_advertised_sunjce_mac_is_computable` does run that
/// direction, but over a hand-list of **6** of the 12 implemented names —
/// so the six added on 2026-08-12 (`HmacSHA512/224`, `HmacSHA512/256` and
/// the four SHA-3s) can lose their service row with nothing going red. That
/// gap is nominated separately; it is in a file this lane does not own.
///
/// ## The population below is HotSpot's
///
/// Measured on openjdk 25.0.3+9 (Microsoft-13877124), this host, 2026-08-13:
///
/// ```java
/// for (Provider.Service s : Security.getProvider("SunJCE").getServices())
///     if (s.getType().equals("Mac")) print(s.getAlgorithm());
/// ```
///
/// answers **28** rows. Twelve are computed here; sixteen are refused, and
/// each of the sixteen is a real algorithm a caller has every reason to ask
/// for — which is exactly why "refused" has to be asserted rather than
/// assumed. `mac_refuses_every_algorithm_it_cannot_compute` covers seven of
/// them plus `Poly1305`/`AESCMAC`, which SunJCE does NOT publish and which
/// therefore belong to that test and not to this one.
///
/// ## What makes it red now
///
/// * an implemented name losing its arm — as before;
/// * **a refused name gaining one.** That is the reverse direction, and it
///   is the point: implementing `HmacPBESHA256` without adding its SunJCE
///   service row would make `Security.getAlgorithms("Mac")` under-report
///   what `Mac.getInstance` will serve. The failure text says to move the
///   name across and seed the row in the same change.
///
/// **What still would not:** a SunJCE `Mac` row that exists on JDK 26 and
/// not on 25. The population is a dated `getServices()` transcript taken by
/// hand, and the honest close is a generated table, which needs a build this
/// lane cannot run.
#[test]
fn mac_supported_set_matches_the_advertised_sunjce_services() {
    // HotSpot SunJCE `Mac` rows, half 1 of 2: computed here.
    let implemented = [
        "HmacMD5",
        "HmacSHA1",
        "HmacSHA224",
        "HmacSHA256",
        "HmacSHA384",
        "HmacSHA512",
        "HmacSHA512/224",
        "HmacSHA512/256",
        "HmacSHA3-224",
        "HmacSHA3-256",
        "HmacSHA3-384",
        "HmacSHA3-512",
    ];
    // HotSpot SunJCE `Mac` rows, half 2 of 2: refused here. PKCS#12
    // (`HmacPBE*`) and PBMAC1 (`PBEWithHmac*`) are key-derivation
    // constructions, not HMAC-over-a-digest; `SslMac*` is the SSLv3 MAC.
    // Serving any of them would mean a second unverified construction.
    let refused_but_advertised_by_hotspot = [
        "HmacPBESHA1",
        "HmacPBESHA224",
        "HmacPBESHA256",
        "HmacPBESHA384",
        "HmacPBESHA512",
        "HmacPBESHA512/224",
        "HmacPBESHA512/256",
        "PBEWithHmacSHA1",
        "PBEWithHmacSHA224",
        "PBEWithHmacSHA256",
        "PBEWithHmacSHA384",
        "PBEWithHmacSHA512",
        "PBEWithHmacSHA512/224",
        "PBEWithHmacSHA512/256",
        "SslMacMD5",
        "SslMacSHA1",
    ];
    assert_eq!(
        implemented.len() + refused_but_advertised_by_hotspot.len(),
        28,
        "the two halves must partition the 28 `Mac` rows HotSpot 25.0.3+9's \
         SunJCE publishes — if you added a row to one half, you took it from \
         the other, or the transcript is stale"
    );
    for algo in implemented {
        assert!(
            mac_algorithm_supported(algo),
            "{algo} is advertised by provider_chain but refused here"
        );
        assert!(
            mac_compute_hmac(algo, b"k", b"d").is_some(),
            "{algo} is advertised but computes nothing"
        );
        assert!(
            mac_output_length(algo).is_some(),
            "{algo} is advertised but has no output length"
        );
    }
    for algo in refused_but_advertised_by_hotspot {
        assert!(
            !mac_algorithm_supported(algo),
            "{algo} is now computed here, but it is still listed as refused. \
             Real SunJCE publishes it, so implementing it is welcome — move \
             the name into `implemented` above AND add its service row to \
             `jca::provider_chain`'s SunJCE seed in the SAME change. \
             Implemented-but-unadvertised is the quiet half of this defect: \
             `Security.getAlgorithms(\"Mac\")` would under-report what \
             `Mac.getInstance` actually serves, so nothing would ever ask."
        );
    }
    let implemented_upper: Vec<String> = implemented.iter().map(|a| mac_normalise(a)).collect();
    // NOT an agreement with HotSpot — a pin on this VM's own, WIDER,
    // normalisation. Measured on HotSpot 25.0.3+9 today:
    //
    // ```text
    // Mac.getInstance("HMAC-SHA256", SunJCE)
    //   -> NoSuchAlgorithmException: no such algorithm: HMAC-SHA256 for provider SunJCE
    // Mac.getInstance("HmacSHA-256", SunJCE)
    //   -> NoSuchAlgorithmException: no such algorithm: HmacSHA-256 for provider SunJCE
    // Mac.getInstance("hmacsha512/256", SunJCE) -> OK (case folds)
    // Mac.getInstance("HMACSHA3-512", SunJCE)   -> OK (case folds)
    // Mac.getInstance("HmacSHA3256",  SunJCE)
    //   -> NoSuchAlgorithmException: no such algorithm: HmacSHA3256 for provider SunJCE
    // ```
    //
    // So JCA folds CASE but does not strip `-`: `HMAC-SHA224` is a name
    // HotSpot refuses and this VM accepts. That is an over-acceptance, not a
    // wrong MAC — the bytes served under it are the bytes `HmacSHA224` would
    // give — and narrowing it would refuse names some caller in this tree
    // may already rely on, which needs a run to settle. It is recorded here
    // rather than silently frozen, because the assertions below would
    // otherwise read as "this matches the JDK", and they do not.
    for algo in [
        "HMACMD5",
        "hmacsha256",
        "Hmac-SHA512",
        // `HMAC-SHA224` folds onto `HMACSHA224`; `HmacSHA3-224` must NOT,
        // which `mac_normalise_keeps_the_sha512_truncations_distinct`
        // covers from the other side.
        "HMAC-SHA224",
        "hmacsha224",
    ] {
        assert!(
            implemented_upper.contains(&mac_normalise(algo)),
            "case/hyphen normalisation drifted for {algo}"
        );
        assert!(mac_algorithm_supported(algo), "{algo} must resolve");
    }
}

/// The defect this lane closed: every unimplemented name used to be served
/// as HMAC-SHA-256, with `getMacLength()` corroborating it. `HmacSHA224`
/// left this list on 2026-08-12 (W7-39) and the SHA-512 truncations and the
/// four SHA-3s followed the same day — all seven are computed now and pinned
/// to HotSpot's bytes by `hmac_extended_matches_hotspot`.
///
/// What remains is everything that is NOT HMAC-over-a-digest: `HmacPBE*` and
/// `PBEWithHmac*` are PKCS#12 / PBMAC1 constructions, and `Poly1305`,
/// `AESCMAC` and `SslMacSHA1` are different primitives entirely. Serving any
/// of them would mean a second unverified construction.
#[test]
fn mac_refuses_every_algorithm_it_cannot_compute() {
    for algo in [
        "HmacPBESHA256",
        "PBEWithHmacSHA256",
        "SslMacSHA1",
        "Poly1305",
        "AESCMAC",
        "NO-SUCH-MAC",
        "",
    ] {
        assert!(
            !mac_algorithm_supported(algo),
            "{algo} must be refused, not silently served as HMAC-SHA-256"
        );
        assert!(
            mac_compute_hmac(algo, b"k", b"d").is_none(),
            "{algo} must produce no bytes"
        );
        assert!(
            mac_output_length(algo).is_none(),
            "{algo} must report no length — a length is what made the lie self-consistent"
        );
    }
}

/// `HmacSHA512/224` and `HmacSHA512/256` are DISTINCT SunJCE algorithms
/// with their own FIPS 180-4 §5.3.6 initial values. `mac_normalise` must
/// not collapse them onto `HmacSHA512224`/`HmacSHA512256`, because the day
/// someone implements one, the collapse would silently serve it for the
/// other. Both are implemented now, which makes the collapse a live hazard
/// rather than a latent one: `mac_compute_hmac` would return the wrong
/// arm's bytes under the right name.
#[test]
fn mac_normalise_keeps_the_sha512_truncations_distinct() {
    assert_ne!(
        mac_normalise("HmacSHA512/224"),
        mac_normalise("HmacSHA512/256")
    );
    assert_ne!(mac_normalise("HmacSHA512/256"), mac_normalise("HmacSHA512"));
    assert_ne!(mac_normalise("HmacSHA3-256"), mac_normalise("HmacSHA256"));
    // …and the three must genuinely produce three different MACs.
    let (k, d) = (b"key".as_slice(), b"data".as_slice());
    let t224 = mac_compute_hmac("HmacSHA512/224", k, d).unwrap();
    let t256 = mac_compute_hmac("HmacSHA512/256", k, d).unwrap();
    let full = mac_compute_hmac("HmacSHA512", k, d).unwrap();
    assert_ne!(t224, t256);
    assert_ne!(&t256[..], &full[..32]);
    assert_ne!(
        mac_compute_hmac("HmacSHA3-256", k, d).unwrap(),
        mac_compute_hmac("HmacSHA256", k, d).unwrap()
    );
}

// Measured on jdk-25 (`Mac.getInstance(name).init(new SecretKeySpec("key",
// name)).doFinal("The quick brown fox jumps over the lazy dog")`), all
// reported `prov=SunJCE`.
const HOTSPOT_HMAC_SHA224: &str = "88ff8b54675d39b8f72322e65ff945c52d96379988ada25639747e69";
const HOTSPOT_HMAC_SHA512_224: &str =
    "a1afb4f708cb63570639195121785ada3dc615989cc3c73f38e306a3";
const HOTSPOT_HMAC_SHA512_256: &str =
    "7fb65e03577da9151a1016e9c2e514d4d48842857f13927f348588173dca6d89";
const HOTSPOT_HMAC_SHA3_224: &str = "ff6fa8447ce10fb1efdccfe62caf8b640fe46c4fb1007912bf85100f";
const HOTSPOT_HMAC_SHA3_256: &str =
    "8c6e0683409427f8931711b10ca92a506eb1fafa48fadd66d76126f47ac2c333";
const HOTSPOT_HMAC_SHA3_384: &str = concat!(
    "aa739ad9fcdf9be4a04f06680ade7a1bd1e01a0af64accb04366234cf9f6934a",
    "0f8589772f857681fcde8acc256091a2"
);
const HOTSPOT_HMAC_SHA3_512: &str = concat!(
    "237a35049c40b3ef5ddd960b3dc893d8284953b9a4756611b1b61bffcf53edd9",
    "79f93547db714b06ef0a692062c609b70208ab8d4a280ceee40ed8100f293063"
);

/// The SAME seven, keyed with a 200-byte key. A key longer than the block
/// size is hashed down to one before padding, which is the ONLY place the
/// block-size constant changes the answer for a short key — so this is the
/// vector set that actually catches a wrong block size, and it is why the
/// SHA-3 rates (144/136/104/72) had to be measured rather than assumed.
/// Same HotSpot 25 run, `key[i] = (byte) i` for i in 0..200.
#[test]
fn hmac_extended_long_key_matches_hotspot() {
    let key: Vec<u8> = (0u32..200).map(|i| i as u8).collect();
    let data = b"The quick brown fox jumps over the lazy dog";
    for (algo, expected) in [
        (
            "HmacSHA224",
            "7776e9af341cb9e8a3041d658522e693ad0c2362cd00e4c03979ae44",
        ),
        (
            "HmacSHA512/224",
            "624bcaab3a5787fb7546f49dd0f124b6956310bc9852bc4811a0bf53",
        ),
        (
            "HmacSHA512/256",
            "1eb904abb4c9b1c20c6865f02b794eb6f5d6fa545a8a2f851e8db4ba6ab9d820",
        ),
        (
            "HmacSHA3-224",
            "bc92b6b85c768d188bb51c807d9be5f9e7de0f32c1c47ee67761b6db",
        ),
        (
            "HmacSHA3-256",
            "2a48cf931ce513d0b65f67fa1d1376d4d82901de5c39804f0b46bcb99182b53b",
        ),
        (
            "HmacSHA3-384",
            concat!(
                "7d489aae7048186e247eb8695939731ffced8e66c5112737a9c1cb3f666c1af2",
                "2033813750e58242dfa53f1dbd2170ad"
            ),
        ),
        (
            "HmacSHA3-512",
            concat!(
                "a44498c88dbeb3267ec1e380f1bbedde06e99d5dded3719559b88c1d42ff2aa5",
                "7d7f2155e654e8c448e7d3547399176bf458d309403d03ea5bcfeba7b531fdc0"
            ),
        ),
        // The two pre-existing algorithms, on the same long-key path, so a
        // regression in `hmac_generic` itself is not mistaken for a new
        // algorithm's block size being wrong.
        (
            "HmacSHA256",
            "9b75f034ad627c6ac4e0b7bd883b09870009485aa3842a69b412e7aacd33bae9",
        ),
        (
            "HmacSHA512",
            concat!(
                "35c1cc9d221994b5f6fef4cb786d1b2fa9ce383560fa6ee7080ad0801400bf1e",
                "49201749ec16c8dd454458a46e62ef609b2602ebf35b9ff37c00867b9bbea0ed"
            ),
        ),
    ] {
        let got = mac_compute_hmac(algo, &key, data)
            .unwrap_or_else(|| panic!("{algo} computes nothing"));
        let hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected, "{algo} long-key disagrees with HotSpot 25");
    }
}

/// Known-answer vectors for the seven MACs this lane ADDED, every one
/// measured on stock HotSpot 25 (`java -cp . MacKat`, key `"key"` over
/// `"The quick brown fox jumps over the lazy dog"`) rather than derived
/// from a reference implementation here.
///
/// These exist because the block size is the part that cannot be defaulted:
/// SHA-224 keeps SHA-256's 64, the SHA-512 truncations keep 128, and the
/// SHA-3 rate SHRINKS as the digest grows (144/136/104/72). A wrong block
/// size still produces plausible-looking bytes of the right length, so only
/// a HotSpot-measured vector can tell the difference.
#[test]
fn hmac_extended_matches_hotspot() {
    let key = b"key";
    let data = b"The quick brown fox jumps over the lazy dog";
    for (algo, expected) in [
        ("HmacSHA224", HOTSPOT_HMAC_SHA224),
        ("HmacSHA512/224", HOTSPOT_HMAC_SHA512_224),
        ("HmacSHA512/256", HOTSPOT_HMAC_SHA512_256),
        ("HmacSHA3-224", HOTSPOT_HMAC_SHA3_224),
        ("HmacSHA3-256", HOTSPOT_HMAC_SHA3_256),
        ("HmacSHA3-384", HOTSPOT_HMAC_SHA3_384),
        ("HmacSHA3-512", HOTSPOT_HMAC_SHA3_512),
    ] {
        let got = mac_compute_hmac(algo, key, data)
            .unwrap_or_else(|| panic!("{algo} computes nothing"));
        let hex: String = got.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, expected, "{algo} disagrees with HotSpot 25");
        assert_eq!(
            mac_output_length(algo),
            Some(got.len()),
            "{algo}: getMacLength() must match the bytes doFinal() produces"
        );
    }
}

/// Known-answer vectors for every implemented MAC, measured on
/// jdk-25.0.3.9-hotspot (`java.version=25.0.3`) with key `"key"` over
/// `"The quick brown fox jumps over the lazy dog"` — the standard HMAC
/// demonstration vector, and independently checkable against RFC 2104
/// implementations elsewhere.
///
/// Their real job is to catch a future widening that gets a block size
/// wrong: each must keep these exact bytes.
#[test]
fn mac_kats_match_hotspot_25() {
    let key = b"key";
    let data = b"The quick brown fox jumps over the lazy dog";
    for (algo, expected) in [
        ("HmacMD5", "80070713463e7749b90c2dc24911e275"),
        ("HmacSHA1", "de7c9b85b8b78aa6bc8a7a36f70a90701c9db4d9"),
        (
            "HmacSHA224",
            "88ff8b54675d39b8f72322e65ff945c52d96379988ada25639747e69",
        ),
        (
            "HmacSHA256",
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8",
        ),
        (
            "HmacSHA384",
            "d7f4727e2c0b39ae0f1e40cc96f60242d5b7801841cea6fc592c5d3e1ae50700\
             582a96cf35e1e554995fe4e03381c237",
        ),
        (
            "HmacSHA512",
            "b42af09057bac1e2d41708e48a902e09b5ff7f12ab428a4fe86653c73dd248fb\
             82f948a549f7b791a5b41915ee4d1ec3935357e4e2317250d0372afa2ebeeb3a",
        ),
    ] {
        let got = mac_compute_hmac(algo, key, data)
            .unwrap_or_else(|| panic!("{algo} must be implemented"));
        let expected: String = expected.chars().filter(|c| !c.is_whitespace()).collect();
        assert_eq!(hex(&got), expected, "{algo} diverges from HotSpot 25");
        assert_eq!(
            mac_output_length(algo),
            Some(got.len()),
            "{algo}: getMacLength() must agree with the bytes doFinal returns"
        );
    }
}

/// HMAC-SHA-224 specifically, against the two inputs that would catch a
/// wrong block size — which is the one thing about this algorithm that
/// cannot be defaulted, and the reason the retired note gave for not
/// widening the set.
///
/// Both vectors measured on jdk-25.0.3.9-hotspot, and the first is the one
/// `probes/CryptoTrioProbe.java` prints, so a CratonVM run of the probe can
/// be diffed against this file without a second measurement:
///
/// ```text
/// key = 32 x 0x0b, data = "hi"      -> 86d73d8ce8749e5b49d61d6211e3a5493d4623984d9ca51aac19cc51
/// key = 200 x 0x00, data = "hi"     -> 7d3925babc9604281f17372a195e0f0351157cc3aa17f032cbfc38de
/// ```
///
/// The 200-byte key is the load-bearing one: RFC 2104 hashes a key LONGER
/// than the block before padding it, so this vector is wrong for any block
/// size other than 64 — including 28 (the digest size) and 128 (SHA-384/512's
/// block, the neighbouring arms in `mac_compute_hmac`). A 32-byte key
/// exercises neither branch and would pass with all three.
#[test]
fn hmac_sha224_block_size_is_64_not_28_and_not_128() {
    let short_key = [0x0bu8; 32];
    assert_eq!(
        hex(&mac_compute_hmac("HmacSHA224", &short_key, b"hi").expect("implemented")),
        "86d73d8ce8749e5b49d61d6211e3a5493d4623984d9ca51aac19cc51"
    );
    let long_key = [0x00u8; 200];
    assert_eq!(
        hex(&mac_compute_hmac("HmacSHA224", &long_key, b"hi").expect("implemented")),
        "7d3925babc9604281f17372a195e0f0351157cc3aa17f032cbfc38de"
    );
    // 28 bytes out, and `getMacLength()` says so. The retired `_ => 32` arm
    // answered 32 here while serving 32 SHA-256 bytes, so the engine agreed
    // with itself all the way down.
    assert_eq!(mac_output_length("HmacSHA224"), Some(28));
    assert_eq!(
        mac_compute_hmac("HmacSHA224", &short_key, b"hi").map(|v| v.len()),
        Some(28)
    );
    // …and it is NOT HMAC-SHA-256 truncated to 28, which is the substitution
    // a `_ =>` arm would produce and which no length check would catch.
    let sha256 = mac_compute_hmac("HmacSHA256", &short_key, b"hi").expect("implemented");
    let sha224 = mac_compute_hmac("HmacSHA224", &short_key, b"hi").expect("implemented");
    assert_ne!(&sha256[..28], &sha224[..]);
}

// -----------------------------------------------------------------------
// W7-29 — `CertificateFactory.getInstance` answers only what SUN advertises
// -----------------------------------------------------------------------

/// A PRIVATE VM identity with no registry row, so these tests read the
/// armed JDK seed and never the mock-shared `TEST_VM` (0) row that
/// `provider_chain`'s tests clear under their own lock.
const CF_TEST_VM: usize = 0x5EC_CF08;

/// The ratchet, in the shape the `Cipher` lane established with
/// `provider_chain::every_advertised_sunjce_cipher_is_serviceable`: the
/// advertised set and the serviceable set are asserted against each other,
/// so the next person to widen one has to widen the other.
///
/// `Security.getAlgorithms("CertificateFactory")` is `[X.509]` on both
/// HotSpot 25 and CratonVM — measured, and they already agreed. The gap was
/// the direction a census does not enumerate: `getInstance` accepted every
/// other name too, so `getInstance("PKCS7").generateCertificate(der)`
/// returned `sun.security.x509.X509CertImpl` (measured: `CN=jcagap`, 714
/// bytes) where HotSpot raises `CertificateException: PKCS7 not found`.
///
/// This depends on the provider-chain seed having run, which is why it
/// builds the registry through `crate::jca::register_jca_natives` exactly as
/// the `KeyManagerFactory`/`TrustManagerFactory` tests above do.
#[test]
fn certificate_factory_serves_exactly_the_advertised_types() {
    let _r = build_registry_with_jca();
    // Advertised by SUN, and every spelling HotSpot resolves — `X509` is
    // `Alg.Alias.CertificateFactory.X509`, and JCA lookup folds case.
    for t in ["X.509", "X509", "x.509", "x509"] {
        assert!(
            certificate_factory_type_supported(CF_TEST_VM, t),
            "{t} is a real SUN CertificateFactory type and must not be refused"
        );
        assert!(
            certificate_factory_stub_serves(t),
            "{t} resolves to X.509, so the synthetic fallback must be allowed to serve it"
        );
    }
    // MUST RAISE. Every one of these was probed on jdk-25.0.3.9-hotspot and
    // answered `CertificateException: <type> not found`; every one of them
    // was answered with a live X.509-parsing factory by the pre-built
    // `target/release/cratonvm.exe` in both default and `--real-jdk` mode.
    for t in [
        "PKCS7",
        "PKCS12",
        "AES",
        "X.500",
        "PkiPath",
        "NO-SUCH-CERT-TYPE",
        "",
    ] {
        assert!(
            !certificate_factory_type_supported(CF_TEST_VM, t),
            "{t} is not advertised by any provider, so getInstance must refuse it rather \
             than hand back an X.509 parser under that name"
        );
    }
}

/// The fallback's own honesty check, kept as a PURE function test so it
/// cannot be voided by provider-registry state the way the ratchet above
/// could: the one-field synthetic `CertificateFactory` parses X.509 DER and
/// nothing else, so it may only stand in for X.509.
///
/// The looseness (`X-509` folds too) is safe only because
/// `certificate_factory_type_supported` gates first and the registry carries
/// no such alias — the assertion below pins that pairing, so a future change
/// that reorders the two gates fails here.
#[test]
fn certificate_factory_stub_only_stands_in_for_x509() {
    for t in ["X.509", "X509", "x.509", "x509", "X-509"] {
        assert!(certificate_factory_stub_serves(t), "{t} folds to X509");
    }
    for t in ["PKCS7", "X.500", "X5090", "509", ""] {
        assert!(
            !certificate_factory_stub_serves(t),
            "{t} must not be served by an X.509 parser"
        );
    }
    assert!(
        !certificate_factory_type_supported(CF_TEST_VM, "X-509"),
        "the registry must not carry an X-509 alias — the stub predicate folds it, and only \
         the registry gate running FIRST keeps that from fabricating a factory"
    );
}
