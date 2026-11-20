use cratonvm_native_api::NativeMethodRegistry;

/// The callback `find` reports for a triple, as a raw address, so two
/// registrations can be compared for identity.
fn cb_addr(r: &NativeMethodRegistry, cls: &str, name: &str, desc: &str) -> Option<usize> {
    r.find(cls, name, desc).map(|cb| cb as usize)
}

const CREATE_ENGINE: (&str, &str, &str) = (
    "javax/net/ssl/SSLContext",
    "createSSLEngine",
    "()Ljavax/net/ssl/SSLEngine;",
);
const CREATE_ENGINE_HOSTPORT: (&str, &str, &str) = (
    "javax/net/ssl/SSLContext",
    "createSSLEngine",
    "(Ljava/lang/String;I)Ljavax/net/ssl/SSLEngine;",
);

/// Compatible / strict: `net_phase_e` overwrites p68's `createSSLEngine`,
/// so `ssleng_alloc` never runs and the 7-vs-2 over-allocation W7-49
/// reported cannot happen in this mode.
#[test]
fn net_phase_e_wins_create_ssl_engine_on_the_essential_path() {
    // net_phase_e alone — the reference callback.
    let mut solo = NativeMethodRegistry::new();
    crate::net_phase_e::register_re6_ssl_context(&mut solo);

    // The essential path's real order.
    let mut boot = NativeMethodRegistry::new();
    crate::phases_late::ssl_security::register_p68_ssl(&mut boot);
    crate::net_phase_e::register_re6_ssl_context(&mut boot);

    for (cls, name, desc) in [CREATE_ENGINE, CREATE_ENGINE_HOSTPORT] {
        let want = cb_addr(&solo, cls, name, desc)
            .unwrap_or_else(|| panic!("register_re6_ssl_context must register {name}{desc}"));
        let got = cb_addr(&boot, cls, name, desc)
            .unwrap_or_else(|| panic!("{name}{desc} must be registered after both"));
        assert_eq!(
            got, want,
            "net_phase_e::register_re6_ssl_context must be the LAST writer of \
             {cls}.{name}{desc} on the essential path (lib.rs 18191 then 18214). \
             If p68 wins here, `ssleng_alloc` is live again and it allocates \
             7 slots on a class declaring 2 — see W7-61.",
        );
    }
}

/// Synthetic overlay: `register_phase68_natives` runs AFTER
/// `register_tls_natives` (lib.rs 23713 then 23716, stated there
/// deliberately), so p68's 7-slot map is authoritative in synthetic mode.
/// A renumber of p68's map is therefore NOT inert there.
#[test]
fn p68_ssl_is_the_last_writer_on_the_ssl_engine_surface() {
    let mut solo = NativeMethodRegistry::new();
    crate::phases_late::ssl_security::register_p68_ssl(&mut solo);

    let mut overlay = NativeMethodRegistry::new();
    super::register_tls_natives(&mut overlay);
    crate::phases_late::register_phase68_natives(&mut overlay);

    // Every triple p68 registers on the engine must be p68's afterwards.
    for (name, desc) in [
        ("setUseClientMode", "(Z)V"),
        ("getUseClientMode", "()Z"),
        ("beginHandshake", "()V"),
        ("closeInbound", "()V"),
        ("closeOutbound", "()V"),
        ("isInboundDone", "()Z"),
        ("isOutboundDone", "()Z"),
        ("setEnabledProtocols", "([Ljava/lang/String;)V"),
        ("setEnabledCipherSuites", "([Ljava/lang/String;)V"),
        ("getSession", "()Ljavax/net/ssl/SSLSession;"),
    ] {
        let want = cb_addr(&solo, "javax/net/ssl/SSLEngine", name, desc)
            .unwrap_or_else(|| panic!("register_p68_ssl must register {name}{desc}"));
        let got = cb_addr(&overlay, "javax/net/ssl/SSLEngine", name, desc)
            .unwrap_or_else(|| panic!("{name}{desc} must be registered after both"));
        assert_eq!(
            got, want,
            "register_p68_ssl must be the LAST writer of SSLEngine.{name}{desc} \
             in synthetic mode (lib.rs 23713 then 23716). If tls.rs wins here, \
             the 14-slot map is authoritative and p68's 7-slot map became dead \
             code — which inverts every reachability verdict in W7-61.",
        );
    }
    // …and the allocator with it.
    for (cls, name, desc) in [CREATE_ENGINE, CREATE_ENGINE_HOSTPORT] {
        let want = cb_addr(&solo, cls, name, desc)
            .unwrap_or_else(|| panic!("register_p68_ssl must register {name}{desc}"));
        let got = cb_addr(&overlay, cls, name, desc).unwrap();
        assert_eq!(
            got, want,
            "register_p68_ssl (`ssleng_alloc`, 7 slots) must be the synthetic-mode \
             winner of {cls}.{name}{desc}",
        );
    }
}

/// The Compatible-mode engine is a `sun/security/ssl/SSLEngineImpl`, and
/// `t27_tls` owns that class outright — every triple p68 puts on the
/// abstract `javax/net/ssl/SSLEngine` also exists there, so the abstract
/// map is never reached by a superclass walk from a real engine.
#[test]
fn t27_covers_every_engine_triple_p68_registers_on_the_abstract_class() {
    let mut p68 = NativeMethodRegistry::new();
    crate::phases_late::ssl_security::register_p68_ssl(&mut p68);

    let mut t27 = NativeMethodRegistry::new();
    crate::t27_tls::register_sslengine_real(&mut t27);

    // The 21 triples `register_p68_ssl` registers on the abstract class.
    for (name, desc) in [
        ("setUseClientMode", "(Z)V"),
        ("getUseClientMode", "()Z"),
        ("setNeedClientAuth", "(Z)V"),
        ("getNeedClientAuth", "()Z"),
        ("setWantClientAuth", "(Z)V"),
        ("getWantClientAuth", "()Z"),
        ("setEnabledProtocols", "([Ljava/lang/String;)V"),
        ("getEnabledProtocols", "()[Ljava/lang/String;"),
        ("setEnabledCipherSuites", "([Ljava/lang/String;)V"),
        ("getEnabledCipherSuites", "()[Ljava/lang/String;"),
        ("getSupportedCipherSuites", "()[Ljava/lang/String;"),
        ("getSupportedProtocols", "()[Ljava/lang/String;"),
        ("beginHandshake", "()V"),
        (
            "getHandshakeStatus",
            "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        ),
        (
            "wrap",
            "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        ),
        (
            "unwrap",
            "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        ),
        ("closeOutbound", "()V"),
        ("closeInbound", "()V"),
        ("isOutboundDone", "()Z"),
        ("isInboundDone", "()Z"),
        ("getSession", "()Ljavax/net/ssl/SSLSession;"),
    ] {
        assert!(
            p68.find("javax/net/ssl/SSLEngine", name, desc).is_some(),
            "this list tracks register_p68_ssl's SSLEngine surface; \
             {name}{desc} is no longer on it — update the list, do not delete it",
        );
        assert!(
            t27.find("sun/security/ssl/SSLEngineImpl", name, desc)
                .is_some(),
            "t27_tls must own SSLEngineImpl.{name}{desc}. Without it, a virtual \
             call on the Compatible-mode engine falls through to the hierarchy \
             walk and can reach p68's 7-slot map on the abstract superclass, \
             which writes an Int into `peerHost` — the W7-61 corruption path.",
        );
    }
}
