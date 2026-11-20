use super::*;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};

/// **Which of the two `register_https_session_accessors` functions owns
/// each of the six names.**
///
/// G7. There are two functions with that name — this file's and
/// `net_phase_e`'s — registering the same six triples on the same two
/// classes, and `lib.rs` calls net_phase_e's first (18688) and this file's
/// second (18805), both inside `register_essential_natives_with_shims`.
/// Registration is last-write-wins, so this file's five bodies are live and
/// net_phase_e's five are dead, while `getSSLSession` — the one name this
/// file does not register — stays net_phase_e's.
///
/// net_phase_e's own comment reasons the opposite way and rules the
/// overwrite out by checking only `register_one`. That is exactly the trap
/// HANDOFF-20260814 §5 names: a correct body silently shadowed by a later
/// registrar. This test makes the SPLIT itself executable, so that:
///
///   * adding `getSSLSession` here fails, instead of silently killing
///     net_phase_e's copy (the only one with a body for it); and
///   * removing any of the five here fails, instead of silently reviving
///     net_phase_e's — which reads a different table.
///
/// Asserted on `register_http_url_connection_real` ALONE, which is what
/// makes it a statement about this file rather than about a call order it
/// cannot see.
#[test]
fn this_files_registrar_owns_five_of_the_six_https_session_accessors() {
    use cratonvm_native_api::NativeMethodRegistry;
    let mut r = NativeMethodRegistry::new();
    super::register_http_url_connection_real(&mut r);

    for cls in [
        "sun/net/www/protocol/https/HttpsURLConnectionImpl",
        "javax/net/ssl/HttpsURLConnection",
    ] {
        for (name, desc) in [
            (
                "getServerCertificates",
                "()[Ljava/security/cert/Certificate;",
            ),
            (
                "getLocalCertificates",
                "()[Ljava/security/cert/Certificate;",
            ),
            ("getCipherSuite", "()Ljava/lang/String;"),
            ("getPeerPrincipal", "()Ljava/security/Principal;"),
            ("getLocalPrincipal", "()Ljava/security/Principal;"),
        ] {
            assert!(
                r.find(cls, name, desc).is_some(),
                "{cls}.{name}{desc} must be registered by THIS file. \
                 lib.rs runs this registrar after net_phase_e's, so \
                 dropping it here does not restore the abstract \
                 declaration — it silently hands the door back to \
                 net_phase_e's copy, which answers from a different table \
                 (`https_carrier_session`, not `https_peer_info`)."
            );
        }
        assert!(
            r.find(cls, "getSSLSession", "()Ljava/util/Optional;")
                .is_none(),
            "{cls}.getSSLSession()Ljava/util/Optional; must NOT be \
             registered here. net_phase_e owns it precisely because this \
             file leaves it alone; registering it here would run last and \
             make net_phase_e's the dead copy. If you need to serve it \
             from this file, move the whole family — do not split it \
             further. See G7-1."
        );
    }
}

/// Every method `sun.net.www.protocol.https.HttpsURLConnectionImpl`
/// declares must be answered by CratonVM, or be on the short list of
/// methods that are deliberately left to NPE.
///
/// The carrier `URL.openConnection()` hands back for an `https:` URL is
/// ALLOCATED, not constructed, so its `delegate` field is null — and every
/// method the class declares is `getfield delegate; invokevirtual …`. A
/// declared method with no CratonVM registration is therefore not "falls
/// back to the JDK", it is a guaranteed
/// `NullPointerException: … because "this.delegate" is null`, which is how
/// the entire SSL/TLS + OCSP portion of the Tomcat suite failed on
/// `TomcatBaseTest.methodUrl`'s opening `setUseCaches(false)`.
///
/// The list is `javap -p --module java.base
/// sun.net.www.protocol.https.HttpsURLConnectionImpl` on Temurin JDK 25,
/// minus `<init>` and the static `checkURL`. It is a constant because this
/// test cannot read the JDK image; a JDK that adds a forwarder will not
/// fail here, it will fail in the suite — which is exactly why the list
/// carries the command that regenerates it.
#[test]
fn every_declared_https_impl_method_is_answered_or_explicitly_refused() {
    use cratonvm_native_api::NativeMethodRegistry;
    const CLS: &str = "sun/net/www/protocol/https/HttpsURLConnectionImpl";

    // Declared on the Impl and left UNREGISTERED on purpose.
    //
    // `setNewClient`/`setProxiedClient` drive the JDK's own `sun.net.www`
    // HTTP client, which `perform` replaces wholesale: there is no
    // superclass body to forward to (the Impl declares them alone) and a
    // no-op would claim a client was reconfigured when nothing happened.
    // The NPE is the honest answer — this path is not implemented.
    const DELIBERATELY_UNREGISTERED: &[(&str, &str)] = &[
        ("setNewClient", "(Ljava/net/URL;)V"),
        ("setNewClient", "(Ljava/net/URL;Z)V"),
        ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;I)V"),
        ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;IZ)V"),
    ];

    // Answered by `net_phase_e`'s registrar, not this file's — see the G7
    // note on `register_https_session_accessors`.
    const OWNED_BY_NET_PHASE_E: &[(&str, &str)] =
        &[("getSSLSession", "()Ljava/util/Optional;")];

    const DECLARED: &[(&str, &str)] = &[
        ("setNewClient", "(Ljava/net/URL;)V"),
        ("setNewClient", "(Ljava/net/URL;Z)V"),
        ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;I)V"),
        ("setProxiedClient", "(Ljava/net/URL;Ljava/lang/String;IZ)V"),
        ("connect", "()V"),
        ("isConnected", "()Z"),
        ("setConnected", "(Z)V"),
        ("getCipherSuite", "()Ljava/lang/String;"),
        (
            "getLocalCertificates",
            "()[Ljava/security/cert/Certificate;",
        ),
        (
            "getServerCertificates",
            "()[Ljava/security/cert/Certificate;",
        ),
        ("getPeerPrincipal", "()Ljava/security/Principal;"),
        ("getLocalPrincipal", "()Ljava/security/Principal;"),
        ("getOutputStream", "()Ljava/io/OutputStream;"),
        ("getInputStream", "()Ljava/io/InputStream;"),
        ("getErrorStream", "()Ljava/io/InputStream;"),
        ("disconnect", "()V"),
        ("usingProxy", "()Z"),
        ("getHeaderFields", "()Ljava/util/Map;"),
        ("getHeaderField", "(Ljava/lang/String;)Ljava/lang/String;"),
        ("getHeaderField", "(I)Ljava/lang/String;"),
        ("getHeaderFieldKey", "(I)Ljava/lang/String;"),
        (
            "setRequestProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        ),
        (
            "addRequestProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V",
        ),
        ("getResponseCode", "()I"),
        (
            "getRequestProperty",
            "(Ljava/lang/String;)Ljava/lang/String;",
        ),
        ("getRequestProperties", "()Ljava/util/Map;"),
        ("setInstanceFollowRedirects", "(Z)V"),
        ("getInstanceFollowRedirects", "()Z"),
        ("setRequestMethod", "(Ljava/lang/String;)V"),
        ("getRequestMethod", "()Ljava/lang/String;"),
        ("getResponseMessage", "()Ljava/lang/String;"),
        ("getHeaderFieldDate", "(Ljava/lang/String;J)J"),
        ("getPermission", "()Ljava/security/Permission;"),
        ("getURL", "()Ljava/net/URL;"),
        ("getContentLength", "()I"),
        ("getContentLengthLong", "()J"),
        ("getContentType", "()Ljava/lang/String;"),
        ("getContentEncoding", "()Ljava/lang/String;"),
        ("getExpiration", "()J"),
        ("getDate", "()J"),
        ("getLastModified", "()J"),
        ("getHeaderFieldInt", "(Ljava/lang/String;I)I"),
        ("getHeaderFieldLong", "(Ljava/lang/String;J)J"),
        ("getContent", "()Ljava/lang/Object;"),
        ("getContent", "([Ljava/lang/Class;)Ljava/lang/Object;"),
        ("toString", "()Ljava/lang/String;"),
        ("setDoInput", "(Z)V"),
        ("getDoInput", "()Z"),
        ("setDoOutput", "(Z)V"),
        ("getDoOutput", "()Z"),
        ("setAllowUserInteraction", "(Z)V"),
        ("getAllowUserInteraction", "()Z"),
        ("setUseCaches", "(Z)V"),
        ("getUseCaches", "()Z"),
        ("setIfModifiedSince", "(J)V"),
        ("getIfModifiedSince", "()J"),
        ("getDefaultUseCaches", "()Z"),
        ("setDefaultUseCaches", "(Z)V"),
        ("equals", "(Ljava/lang/Object;)Z"),
        ("hashCode", "()I"),
        ("setConnectTimeout", "(I)V"),
        ("getConnectTimeout", "()I"),
        ("setReadTimeout", "(I)V"),
        ("getReadTimeout", "()I"),
        ("setFixedLengthStreamingMode", "(I)V"),
        ("setFixedLengthStreamingMode", "(J)V"),
        ("setChunkedStreamingMode", "(I)V"),
        ("setAuthenticator", "(Ljava/net/Authenticator;)V"),
        ("getSSLSession", "()Ljava/util/Optional;"),
    ];

    let mut r = NativeMethodRegistry::new();
    super::register_http_url_connection_real(&mut r);

    let mut missing: Vec<String> = Vec::new();
    for &(name, desc) in DECLARED {
        if DELIBERATELY_UNREGISTERED.contains(&(name, desc))
            || OWNED_BY_NET_PHASE_E.contains(&(name, desc))
        {
            assert!(
                r.find(CLS, name, desc).is_none(),
                "{CLS}.{name}{desc} is on an exemption list but IS registered \
                 here — move it out of the list, or out of this registrar."
            );
            continue;
        }
        if r.find(CLS, name, desc).is_none() {
            missing.push(format!("{name}{desc}"));
        }
    }
    assert!(
        missing.is_empty(),
        "{} declared method(s) of {CLS} reach the JDK's own \
         `getfield delegate; invokevirtual …` body on a carrier whose \
         `delegate` is null, i.e. throw NullPointerException: {missing:?}. \
         Add them to `register_https_delegate_forwarders` (forwarding to \
         the superclass body the Impl overrides), or state why the NPE is \
         the right answer and list them in DELIBERATELY_UNREGISTERED.",
        missing.len()
    );
}

/// rustls's TLS 1.3 spelling is not JSSE's, and `getCipherSuite()` is
/// contracted to answer JSSE's. Both directions asserted: the five
/// `TLS13_*` variants are rewritten, and a TLS 1.2 name — where the two
/// already agree — must pass through untouched. Oracle for the expected
/// strings: `scratchpad/c12/C12Probe.java` §B on HotSpot 25.
#[test]
fn tls13_suite_names_are_reported_with_jsse_spelling() {
    assert_eq!(
        jsse_cipher_suite_name("TLS13_AES_256_GCM_SHA384"),
        "TLS_AES_256_GCM_SHA384"
    );
    assert_eq!(
        jsse_cipher_suite_name("TLS13_AES_128_GCM_SHA256"),
        "TLS_AES_128_GCM_SHA256"
    );
    assert_eq!(
        jsse_cipher_suite_name("TLS13_CHACHA20_POLY1305_SHA256"),
        "TLS_CHACHA20_POLY1305_SHA256"
    );
    assert_eq!(
        jsse_cipher_suite_name("TLS13_AES_128_CCM_8_SHA256"),
        "TLS_AES_128_CCM_8_SHA256"
    );
    // TLS 1.2: already the registry name on both sides.
    assert_eq!(
        jsse_cipher_suite_name("TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"),
        "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
    );
    // Not a blanket "TLS" rewrite: only the prefix, and only when present.
    assert_eq!(jsse_cipher_suite_name("UNKNOWN"), "UNKNOWN");
}

/// SOURCE WITNESS — the session capture must stay ABOVE the early return.
///
/// `huc_verify_hostname` ends STEP 1 with `if builtin.is_ok() { return
/// Ok(()); }`, and that return is the path EVERY SUCCESSFUL REQUEST TAKES.
/// A `record_https_carrier_session` call below it would record a session
/// only for connections whose built-in hostname check FAILED — i.e. it
/// would pass any probe that deliberately breaks verification and capture
/// nothing in production, leaving all six `HttpsURLConnection` session
/// accessors answering `IllegalStateException: connection not yet open`
/// forever. No behavioural test can see that difference without a live TLS
/// peer, so the ordering is asserted against the source.
///
/// Reads the WORKING TREE rather than an `include_str!` snapshot, so it
/// tracks the file someone is editing, and skips rather than fails if the
/// source is not on disk (a packaged build). Line endings are normalised
/// because this repository is edited from both Windows and Linux.
#[test]
fn the_session_capture_precedes_the_success_path_early_return() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("http_url_connection.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("http_url_connection.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();

    let fn_start = lines
        .iter()
        .position(|l| l.starts_with("fn huc_verify_hostname("))
        .expect("huc_verify_hostname must still exist");
    // The function body ends at the next top-level `}`.
    let fn_end = lines
        .iter()
        .enumerate()
        .skip(fn_start)
        .find(|(_, l)| **l == "}")
        .map(|(i, _)| i)
        .expect("huc_verify_hostname must be terminated");
    let body = &lines[fn_start..fn_end];

    let capture = body
        .iter()
        .position(|l| l.contains("record_https_carrier_session("))
        .expect(
            "huc_verify_hostname must record the negotiated session; without it every \
             HttpsURLConnection session accessor answers \"connection not yet open\"",
        );
    let early_return = body
        .iter()
        .position(|l| l.trim() == "if builtin.is_ok() {")
        .expect("STEP 1's success-path early return must still be recognisable");

    assert!(
        capture < early_return,
        "record_https_carrier_session is at body line {capture}, BELOW the \
         `if builtin.is_ok()` early return at body line {early_return} — that is the \
         path every successful request takes, so the capture would only ever fire for \
         connections whose hostname check FAILED. Move it back above STEP 1."
    );
}

/// SOURCE WITNESS — G44 N1: the verifier gets THE carrier's session, and
/// the local mint is only ever the fallback.
///
/// MEASURED, `RSslLiveSession` on `9ae371468`:
/// `verifier.sameObjectAsGetSSLSession = false  WANT true`. HotSpot hands
/// `HostnameVerifier.verify` the same object `getSSLSession()` returns
/// afterwards, and two minters cannot satisfy that however identical their
/// field writes are — which is why the fix is a lookup and not a fifth copy
/// of the same four `set_field` calls.
///
/// Asserted against the source for the same reason the witness above is:
/// the difference between one object and two is only observable with a live
/// TLS peer, so no `MockNativeContext` test can see it. What IS checkable
/// here is the shape — that `huc_verify_hostname` consults
/// `https_carrier_session_object` and that the only remaining
/// `try_alloc_concurrent_synthetic` of a session in this file sits inside
/// the fallback helper, not in the verifier path itself.
#[test]
fn the_verifier_is_handed_the_carriers_session_not_a_second_one() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("http_url_connection.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("http_url_connection.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let fn_start = lines
        .iter()
        .position(|l| l.starts_with("fn huc_verify_hostname("))
        .expect("huc_verify_hostname must still exist");
    let fn_end = lines
        .iter()
        .enumerate()
        .skip(fn_start)
        .find(|(_, l)| **l == "}")
        .map(|(i, _)| i)
        .expect("huc_verify_hostname must be terminated");
    // CODE lines only. This function's body is more comment than code, and
    // both assertions below would otherwise be satisfied — or broken — by
    // prose that merely names the function.
    let body: Vec<&str> = lines[fn_start..fn_end]
        .iter()
        .copied()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect();

    assert!(
        body.iter()
            .any(|l| l.contains("https_carrier_session_object(")),
        "huc_verify_hostname must ask net_phase_e for THE session this carrier already \
         handed out. Minting a private one here is what made \
         `verifier.sameObjectAsGetSSLSession` answer false."
    );
    assert!(
        !body
            .iter()
            .any(|l| l.contains("try_alloc_concurrent_synthetic(")),
        "huc_verify_hostname must not allocate an SSLSession itself — the fallback lives \
         in huc_mint_verifier_session, so that the carrier's session is the DEFAULT and \
         the private mint is the exception."
    );
}

/// `disconnect()` tears down the connection-level view of BOTH https
/// session tables — and leaves this file's row in place rather than
/// deleting it.
///
/// MEASURED, HotSpot (`G7-1` §1d): after `disconnect()` all six accessors
/// throw `IllegalStateException: connection not yet open` again, the same
/// exception a never-handshaked connection throws. Two halves are asserted
/// here because each is a separate way to get this wrong:
///
///   * the row must SURVIVE, flagged. Removing it would put
///     `https_ensure_exchanged` back on its "never handshaked" path, and
///     the very next accessor would re-issue the HTTPS request over the
///     network and answer from the fresh entry — a wrong answer AND a
///     second request;
///   * `net_phase_e`'s carrier session must go, because `getSSLSession` is
///     the one of the six that reads THAT table (`G7-1` §5.1). Recycling
///     one table and not the other leaves the six disagreeing about whether
///     the connection is open.
/// G51-1 N2 — draining the response body recycles the connection's view,
/// the way HotSpot's `KeepAliveCache` does at the same instant.
///
/// Drives the observer directly rather than through `native-io`: the
/// dispatch sites are that crate's and already have their own tests
/// (`native-io/src/lib.rs`, the `BaisEvent` recorder). What is this file's
/// to prove is that the observer maps a stream back to its carrier, that
/// it recycles exactly once, and that an unregistered stream is inert.
#[test]
fn draining_the_response_body_recycles_the_carrier() {
    use cratonvm_native_api::registry::BaisEvent;
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let stream = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let chain = vec![vec![0x30u8, 0x01, 0x02]];

    record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_256_GCM_SHA384");
    note_response_stream(&ctx, stream, Some(carrier));
    // The rows are filed under the objects' weak lock keys (gc-common w27-b).
    let key = huc_existing_obj_key(&ctx, carrier).expect("the handshake was filed");
    let stream_key = huc_existing_obj_key(&ctx, stream).expect("the stream was noted");
    assert!(
        https_peer_info()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|i| !i.recycled),
        "a completed exchange starts OPEN"
    );

    huc_live_bais_event(&mut ctx, stream, BaisEvent::Eof).expect("the observer must not fail");
    assert!(
        https_peer_info()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|i| i.recycled),
        "at body EOF the CONNECTION-level view is torn down — every accessor throws \
         IllegalStateException: connection not yet open again"
    );
    assert!(
        https_peer_info().lock().unwrap().contains_key(&key),
        "the ROW must survive: https_ensure_exchanged reads a missing entry as \
         \"never handshaked\" and would re-issue the request over the network"
    );

    // Eof fires on EVERY exhausted read and a closed stream produces Close
    // as well, so the second and third events must find nothing to do.
    assert!(
        https_response_streams()
            .lock()
            .unwrap()
            .get(&stream_key)
            .is_none(),
        "the association is consumed by the first event"
    );
    huc_live_bais_event(&mut ctx, stream, BaisEvent::Close).expect("idempotent");
}

/// A stream that was never associated with an `https` carrier must be
/// inert. Every `ByteArrayInputStream` in the process reaches this
/// observer — a plain `http:` body, an application's own buffer, a
/// resource read through `URLClassLoader` — and recycling anything for
/// those would tear down state they have nothing to do with.
#[test]
fn an_unassociated_stream_recycles_nothing() {
    use cratonvm_native_api::registry::BaisEvent;
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let stranger = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);

    record_https_peer_info(
        &ctx,
        Some(carrier),
        &[vec![0x30u8]],
        "TLS_AES_128_GCM_SHA256",
    );
    let key = huc_existing_obj_key(&ctx, carrier).expect("the handshake was filed");
    // Deliberately NOT noted, and noted with no carrier — both are the
    // shapes an ordinary BAIS arrives in.
    note_response_stream(&ctx, stranger, None);
    huc_live_bais_event(&mut ctx, stranger, BaisEvent::Eof).expect("inert");
    assert!(
        https_peer_info()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|i| !i.recycled),
        "an unrelated stream's EOF must not recycle a live connection"
    );
}

#[test]
fn disconnect_recycles_both_https_session_tables() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let carrier = ctx.alloc_object(cratonvm_types::ClassId::new(0), 4);
    let chain = vec![vec![0x30u8, 0x01, 0x02]];

    record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_256_GCM_SHA384");
    let key = huc_existing_obj_key(&ctx, carrier).expect("the handshake was filed");
    crate::net_phase_e::record_https_carrier_session(
        &mut ctx,
        carrier,
        "TLSv1.3",
        "TLS_AES_256_GCM_SHA384",
        &chain,
        "example.test",
        443,
    );
    assert!(
        https_peer_info()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|i| !i.recycled),
        "a completed exchange starts OPEN"
    );

    https_recycle_carrier(&mut ctx, carrier);

    let table = https_peer_info().lock().unwrap();
    let info = table
        .get(&key)
        .expect("the row must survive the recycle — see this test's doc comment");
    assert!(info.recycled, "the row must be flagged, not merely present");
    assert_eq!(
        info.cipher, "TLS_AES_256_GCM_SHA384",
        "recycling reports the connection closed; it does not forge the handshake's data"
    );
    drop(table);
    assert!(
        crate::net_phase_e::https_carrier_session_object(&mut ctx, carrier).is_none(),
        "getSSLSession's table must have been evicted too, or five accessors report \
         `connection not yet open` while the sixth still hands out a session"
    );

    // Idempotent: `disconnect()` is documented as callable twice, and the
    // second call must not release a global root a second time.
    https_recycle_carrier(&mut ctx, carrier);

    // A connection that genuinely re-handshakes is OPEN again — the flag is
    // cleared by the recorder, not sticky on the carrier's identity.
    record_https_peer_info(&ctx, Some(carrier), &chain, "TLS_AES_128_GCM_SHA256");
    assert!(
        https_peer_info()
            .lock()
            .unwrap()
            .get(&key)
            .is_some_and(|i| !i.recycled),
        "re-recording a handshake must clear the recycled flag"
    );
    https_peer_info().lock().unwrap().remove(&key);
}

/// SOURCE WITNESS — `https_ensure_exchanged`'s early return must stay a
/// presence test, not a liveness test.
///
/// It is the one line that keeps a recycled carrier from re-issuing its
/// HTTPS request: "no entry" means "never handshaked, go and handshake". A
/// later change that made this read `.get(..).is_some_and(|i| !i.recycled)`
/// — which is exactly the shape the three ACCESSORS were just given, so it
/// looks like consistency — would drive a second network request from the
/// next accessor call and repopulate the table with a live entry.
#[test]
fn the_lazy_exchange_guard_is_a_presence_test_not_a_liveness_test() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("http_url_connection.rs");
    let Ok(src) = std::fs::read_to_string(&path) else {
        println!("http_url_connection.rs not on disk at {path:?}; witness skipped");
        return;
    };
    let lines: Vec<&str> = src.lines().map(|l| l.trim_end_matches('\r')).collect();
    let fn_start = lines
        .iter()
        // `_body` since `WORKER-5-NOTE-13`: `https_ensure_exchanged` is now
        // the `&mut ObjectRef` wrapper and the guard this witness is about
        // lives in the body half. `expect` keeps a further rename loud.
        .position(|l| l.starts_with("fn https_ensure_exchanged_body("))
        .expect("https_ensure_exchanged_body must still exist");
    let fn_end = lines
        .iter()
        .enumerate()
        .skip(fn_start)
        .find(|(_, l)| **l == "}")
        .map(|(i, _)| i)
        .expect("https_ensure_exchanged_body must be terminated");
    let body = &lines[fn_start..fn_end];

    assert!(
        body.iter().any(|l| l.contains("contains_key(")),
        "https_ensure_exchanged's guard must be a plain presence test"
    );
    assert!(
        !body.iter().any(|l| l.contains("recycled")),
        "https_ensure_exchanged must NOT skip recycled rows — treating a recycled row as \
         absent makes the next accessor re-issue the HTTPS request and answer from the \
         fresh entry. The flag is read by the accessors, never by this guard."
    );
}

/// A pooled keep-alive connection must actually be CLOSED once it is past
/// `POOL_IDLE_WINDOW`, not merely become ineligible for reuse.
///
/// Before the reaper existed, `pool_take` was the only thing that dropped a
/// stale entry, so a client that made one request and stopped held its
/// socket open for the life of the VM — visible to the peer, and what broke
/// `MockWebServer.close()` (see `pool_start_reaper`). This asserts from the
/// server side, which is the side that noticed.
#[test]
fn pooled_connection_is_closed_once_idle() {
    use std::io::Read;
    use std::net::{TcpListener, TcpStream as Stream};

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let client = Stream::connect(("127.0.0.1", port)).expect("connect");
    let (mut accepted, _) = listener.accept().expect("accept");

    pool_put("127.0.0.1", port, client);

    // The peer must observe EOF within a bounded window: the reaper only
    // discards entries older than POOL_IDLE_WINDOW, so allow that plus
    // several sweep ticks of slack.
    accepted
        .set_read_timeout(Some(POOL_IDLE_WINDOW + Duration::from_secs(5)))
        .expect("set_read_timeout");
    let mut buf = [0u8; 1];
    let observed = accepted.read(&mut buf);
    assert!(
        matches!(observed, Ok(0)),
        "peer should see EOF after the pooled connection goes idle, got {observed:?}"
    );
}

/// A `localhost` leaf with `SAN dNSName=localhost` (plus `foo.test`,
/// `bar.test`, `IP 127.0.0.1`) — the same fixture the rustls loopback
/// self-test serves, and the same shape every Spring Boot / Tomcat TLS
/// test's keystore presents.
fn localhost_leaf_der() -> Vec<u8> {
    let pem = include_str!("../t27_certs/server.crt");
    crate::t27_tls::parse_cert_chain_pem(pem)
        .expect("t27_certs/server.crt must parse")
        .remove(0)
        .as_ref()
        .to_vec()
}

/// The regression this whole change exists for.
///
/// The real JDK's `HttpsURLConnection.<clinit>` installs
/// `HttpsURLConnection$DefaultHostnameVerifier`, whose entire body is
/// `iconst_0; ireturn`, and every `HttpsURLConnection` instance inherits it
/// through the constructor. Failing to recognise that class as a default
/// stand-in — as the first version of this code did, which knew only about
/// the VM's own bare-interface synthetic — turns its unconditional `false`
/// into a rejection of every single https request.
///
/// Deliberately spelled out as literals rather than derived: this is a
/// name-matching contract with the JDK (real JSSE matches the same class by
/// canonical name in `HttpsClient.afterConnect`), so a rename must break a
/// test rather than silently fall through to "app verifier".
#[test]
fn jdk_default_hostname_verifier_is_recognised_as_a_non_check() {
    assert!(is_default_hostname_verifier(Some(
        "javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier"
    )));
    assert!(is_default_hostname_verifier(Some(
        "javax/net/ssl/HostnameVerifier"
    )));
    // NOT `None`. By the time a name reaches this predicate the caller has
    // already returned for "no verifier installed at all"
    // (`let Some(verifier0) = verifier0 else { ... }`), so `None` here means
    // "a verifier object exists whose class this VM could not name" — a
    // lambda, which is an APPLICATION verifier and must be consulted.
    // `an_unnameable_verifier_is_not_a_default_stand_in` is the regression
    // guard for exactly that, with the HotSpot measurement behind it; this
    // line used to assert the opposite and the two directly contradicted
    // each other.
    //
    // Anything else is a genuine application verifier and must be
    // consulted (as a fallback) rather than skipped.
    assert!(!is_default_hostname_verifier(Some("com/example/PinningHV")));
    assert!(!is_default_hostname_verifier(Some(
        "org/apache/http/conn/ssl/NoopHostnameVerifier"
    )));
}

/// The built-in RFC 2818 check must ACCEPT the ordinary loopback case on
/// its own, before any verifier is consulted — that is what makes the
/// verifier a fallback rather than a gate. If this ever returns `Err`, the
/// JDK default verifier's hardcoded `false` becomes the deciding answer
/// again and every https request fails.
#[test]
fn builtin_endpoint_identification_accepts_a_localhost_leaf() {
    let chain = vec![localhost_leaf_der()];
    assert_eq!(
        huc_builtin_endpoint_identification("localhost", &chain),
        Ok(())
    );
    // Case-insensitive, per RFC 6125.
    assert_eq!(
        huc_builtin_endpoint_identification("LOCALHOST", &chain),
        Ok(())
    );
    // The same leaf's IP SAN.
    assert_eq!(
        huc_builtin_endpoint_identification("127.0.0.1", &chain),
        Ok(())
    );
}

/// ...and must REJECT a host the leaf does not assert, so the fallback to
/// the installed verifier is actually reachable. A check that always
/// passed would silently disable app-supplied verifiers altogether.
#[test]
fn builtin_endpoint_identification_rejects_a_foreign_host_and_an_empty_chain() {
    let chain = vec![localhost_leaf_der()];
    assert!(huc_builtin_endpoint_identification("evil.example.com", &chain).is_err());
    assert!(huc_builtin_endpoint_identification("localhost", &[]).is_err());
    assert!(huc_builtin_endpoint_identification("localhost", &[vec![0u8; 8]]).is_err());
}

#[test]
fn test_register_http_url_connection_init() {
    let mut r = NativeMethodRegistry::new();
    register_http_url_connection_real(&mut r);
    assert!(r
        .find(
            "sun/net/www/protocol/http/HttpURLConnection",
            "<init>",
            "(Ljava/net/URL;)V"
        )
        .is_some());
}

#[test]
fn test_register_https_url_connection_impl_init() {
    let mut r = NativeMethodRegistry::new();
    register_http_url_connection_real(&mut r);
    assert!(r
        .find(
            "sun/net/www/protocol/https/HttpsURLConnectionImpl",
            "<init>",
            "(Ljava/net/URL;)V"
        )
        .is_some());
}

#[test]
fn test_register_huc_connect_methods() {
    let mut r = NativeMethodRegistry::new();
    register_http_url_connection_real(&mut r);
    let cls = "sun/net/www/protocol/http/HttpURLConnection";
    assert!(r.find(cls, "connect", "()V").is_some());
    assert!(r.find(cls, "getResponseCode", "()I").is_some());
    assert!(r
        .find(cls, "getInputStream", "()Ljava/io/InputStream;")
        .is_some());
    assert!(r
        .find(cls, "getOutputStream", "()Ljava/io/OutputStream;")
        .is_some());
    assert!(r.find(cls, "disconnect", "()V").is_some());
}

#[test]
fn test_register_huc_header_methods() {
    let mut r = NativeMethodRegistry::new();
    register_http_url_connection_real(&mut r);
    let cls = "sun/net/www/protocol/http/HttpURLConnection";
    assert!(r
        .find(
            cls,
            "getHeaderField",
            "(Ljava/lang/String;)Ljava/lang/String;"
        )
        .is_some());
    assert!(r
        .find(cls, "getHeaderField", "(I)Ljava/lang/String;")
        .is_some());
    assert!(r
        .find(cls, "getHeaderFieldKey", "(I)Ljava/lang/String;")
        .is_some());
    assert!(r.find(cls, "getContentLength", "()I").is_some());
    assert!(r.find(cls, "getContentLengthLong", "()J").is_some());
}

#[test]
fn test_register_huc_request_property_methods() {
    let mut r = NativeMethodRegistry::new();
    register_http_url_connection_real(&mut r);
    let cls = "sun/net/www/protocol/http/HttpURLConnection";
    assert!(r
        .find(
            cls,
            "setRequestProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V"
        )
        .is_some());
    assert!(r
        .find(
            cls,
            "addRequestProperty",
            "(Ljava/lang/String;Ljava/lang/String;)V"
        )
        .is_some());
    assert!(r
        .find(cls, "setRequestMethod", "(Ljava/lang/String;)V")
        .is_some());
    assert!(r
        .find(cls, "getRequestMethod", "()Ljava/lang/String;")
        .is_some());
}

#[test]
fn test_parse_url_http() {
    let p = parse_url("http://example.com/foo").unwrap();
    assert_eq!(p.scheme, "http");
    assert_eq!(p.host, "example.com");
    assert_eq!(p.port, 80);
    assert_eq!(p.path, "/foo");
}

#[test]
fn test_parse_url_https_with_port() {
    let p = parse_url("https://example.com:8443").unwrap();
    assert_eq!(p.scheme, "https");
    assert_eq!(p.host, "example.com");
    assert_eq!(p.port, 8443);
    assert_eq!(p.path, "/");
}

#[test]
fn test_parse_url_query_only_and_fragment() {
    let p = parse_url("http://localhost:8080?trace=false&message=false").unwrap();
    assert_eq!(p.host, "localhost");
    assert_eq!(p.port, 8080);
    assert_eq!(p.path, "/?trace=false&message=false");

    let p = parse_url("https://example.com:8443#client-only").unwrap();
    assert_eq!(p.host, "example.com");
    assert_eq!(p.port, 8443);
    assert_eq!(p.path, "/");
}

#[test]
fn test_parse_url_rejects_unknown_scheme() {
    assert!(parse_url("ftp://example.com").is_err());
}

#[test]
fn test_parse_url_rejects_empty_host() {
    assert!(parse_url("http:///foo").is_err());
}

#[test]
fn test_build_request_get() {
    let p = parse_url("http://example.com/foo").unwrap();
    let req = build_request("GET", &p, &[], &[]);
    let s = String::from_utf8(req).unwrap();
    assert!(s.starts_with("GET /foo HTTP/1.1\r\n"));
    assert!(s.contains("Host: example.com\r\n"));
    assert!(s.contains("User-Agent: Java/CratonVM\r\n"));
    assert!(s.contains("Connection: keep-alive\r\n"));
}

#[test]
fn test_build_request_post_body() {
    let p = parse_url("https://example.com:8443/api").unwrap();
    let req = build_request(
        "POST",
        &p,
        &[("Content-Type".to_string(), "application/json".to_string())],
        b"{}",
    );
    let s = String::from_utf8(req).unwrap();
    assert!(s.contains("POST /api HTTP/1.1\r\n"));
    assert!(s.contains("Host: example.com:8443\r\n"));
    assert!(s.contains("Content-Length: 2\r\n"));
    assert!(s.contains("Content-Type: application/json\r\n"));
    assert!(s.ends_with("{}"));
}

#[test]
fn test_parse_url_userinfo_stripped() {
    // `http://alice:secret@localhost:8080/resource` — the user-info must be
    // stripped from host/port (connect target, Host header) and surfaced in
    // `userinfo` (ResourceTests.useUserInfoToSetBasicAuth).
    let p = parse_url("http://alice:secret@localhost:8080/resource").unwrap();
    assert_eq!(p.scheme, "http");
    assert_eq!(p.host, "localhost");
    assert_eq!(p.port, 8080);
    assert_eq!(p.path, "/resource");
    assert_eq!(p.userinfo.as_deref(), Some("alice:secret"));
    // No user-info → None.
    assert_eq!(parse_url("http://example.com/x").unwrap().userinfo, None);
}

#[test]
fn test_build_request_preemptive_basic_auth_from_userinfo() {
    let p = parse_url("http://alice:secret@localhost:8080/resource").unwrap();
    let req = build_request("GET", &p, &[], b"");
    let s = String::from_utf8(req).unwrap();
    // Host header must NOT carry the user-info.
    assert!(s.contains("Host: localhost:8080\r\n"));
    // base64("alice:secret") == "YWxpY2U6c2VjcmV0" (preemptive Basic auth).
    assert!(s.contains("Authorization: Basic YWxpY2U6c2VjcmV0\r\n"));

    // An explicitly staged Authorization header wins over the user-info.
    let req2 = build_request(
        "GET",
        &p,
        &[("Authorization".to_string(), "Bearer tok".to_string())],
        b"",
    );
    let s2 = String::from_utf8(req2).unwrap();
    assert!(s2.contains("Authorization: Bearer tok\r\n"));
    assert!(!s2.contains("Basic YWxpY2U6c2VjcmV0"));
}

#[test]
fn test_content_length_prefers_header_over_body() {
    // HEAD responses advertise the entity size in Content-Length but carry
    // no body — the header must win (ResourceTests.remoteResourceExists).
    let headers = vec![("Content-Length".to_string(), "6".to_string())];
    assert_eq!(content_length_of(&headers, 0), 6);
    // NO header → -1, and the buffered body size is not a substitute for
    // it. This line asserted `5` — the body size — until 2026-09-11, when
    // `L6HttpLoopbackSweep` measured HotSpot 25.0.4+7 against a loopback
    // server: a chunked 200 answers -1 (rows 59) and a `204 No Content`
    // answers -1 (row 50), where this VM answered the bytes it happened to
    // have buffered and 0. Zero is the answer that matters: it is
    // indistinguishable from a real zero-length entity, and -1 is how the
    // JDK says "unknown, read to EOF".
    assert_eq!(content_length_of(&[], 5), -1);
    // Duplicate headers: the LAST wins (MessageHeader.findValue iterates
    // backwards). MockWebServer emits its bodiless "Content-Length: 0"
    // default before the test's addHeader("Content-Length", "6").
    let dup = vec![
        ("Content-Length".to_string(), "0".to_string()),
        ("Content-Type".to_string(), "text/plain".to_string()),
        ("Content-Length".to_string(), "6".to_string()),
    ];
    assert_eq!(content_length_of(&dup, 0), 6);
}

#[test]
fn test_find_subslice() {
    assert_eq!(find_subslice(b"hello world", b"world"), Some(6));
    assert_eq!(find_subslice(b"abc", b""), None);
}

#[test]
fn test_read_chunked_simple() {
    let mut data: Vec<u8> = Vec::new();
    // Chunk sizes are hex; "in \r\nchunks." is 12 bytes = 0xc.
    // Embedded CRLF inside the chunk data is preserved per RFC 7230 §4.1.
    data.extend_from_slice(b"4\r\nWiki\r\n");
    data.extend_from_slice(b"6\r\npedia \r\n");
    data.extend_from_slice(b"c\r\nin \r\nchunks.\r\n");
    data.extend_from_slice(b"0\r\n\r\n");
    let mut empty: &[u8] = &[];
    let body = read_chunked(&mut data, &mut empty).unwrap();
    assert_eq!(body, b"Wikipedia in \r\nchunks.");
}

#[test]
fn test_read_chunked_retries_raw_eintr() {
    struct InterruptOnce<'a> {
        interrupted: bool,
        remaining: &'a [u8],
    }

    impl Read for InterruptOnce<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::from_raw_os_error(4));
            }
            self.remaining.read(buf)
        }
    }

    let mut prefix = b"4\r\n".to_vec();
    let mut stream = InterruptOnce {
        interrupted: false,
        remaining: b"Wiki\r\n0\r\n\r\n",
    };
    assert_eq!(read_chunked(&mut prefix, &mut stream).unwrap(), b"Wiki");
}

#[test]
fn test_read_response_304_is_bodiless() {
    // RFC 9110 §6.4.1: a 304 has no message body even when it carries a
    // (bogus) Content-Length. read_response must return immediately with an
    // empty body and NOT consume the trailing bytes — otherwise a keep-alive
    // 304 (no real body coming) hangs the client. The trailing "HELLO" here
    // stands in for "bytes that are not ours to read".
    let mut data: &[u8] =
        b"HTTP/1.1 304 Not Modified\r\nETag: W/\"x\"\r\nContent-Length: 5\r\n\r\nHELLO";
    let (status, headers, body) = read_response(&mut data, false).unwrap();
    assert_eq!(status, 304);
    assert!(body.is_empty(), "304 must be bodiless");
    assert!(headers
        .iter()
        .any(|(n, v)| n.eq_ignore_ascii_case("etag") && v == "W/\"x\""));
}

#[test]
fn test_read_response_204_is_bodiless() {
    let mut data: &[u8] = b"HTTP/1.1 204 No Content\r\nContent-Length: 3\r\n\r\nabc";
    let (status, _h, body) = read_response(&mut data, false).unwrap();
    assert_eq!(status, 204);
    assert!(body.is_empty(), "204 must be bodiless");
}

#[test]
fn test_read_response_200_reads_body() {
    // Guard against over-broadening the bodiless rule: a normal 200 with a
    // Content-Length must still return its body.
    let mut data: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nHELLO";
    let (status, _h, body) = read_response(&mut data, false).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body, b"HELLO");
}

#[test]
fn test_read_response_preserves_non_utf8_header_bytes_as_latin1() {
    // Tomcat's RFC6265 cookie test emits U+0120 as UTF-8 (C4 A0) then
    // uses String.getBytes(ISO_8859_1) to recover the wire bytes from the
    // response header. The HTTP client must therefore retain C4 A0 as
    // Latin-1 code points, not decode or replace them as UTF-8.
    let mut data: &[u8] =
        b"HTTP/1.1 200 OK\r\nSet-Cookie: Test=\xC4\xA0\r\nContent-Length: 0\r\n\r\n";
    let (status, headers, body) = read_response(&mut data, false).unwrap();
    assert_eq!(status, 200);
    assert!(body.is_empty());
    assert!(headers
        .iter()
        .any(|(name, value)| name == "Set-Cookie" && value == "Test=\u{00c4}\u{00a0}"));
}

#[test]
fn test_registry_allocates_unique_ids() {
    let mut reg = ConnRegistry::new();
    let id1 = reg.allocate(
        1,
        ConnState {
            status: 200,
            response_body: b"a".to_vec(),
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        },
    );
    let id2 = reg.allocate(
        2,
        ConnState {
            status: 404,
            response_body: b"b".to_vec(),
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        },
    );
    assert_ne!(id1, id2);
    assert_eq!(reg.get(id1).unwrap().status, 200);
    assert_eq!(reg.get(id2).unwrap().status, 404);
}

#[test]
fn test_registry_remove_clears_state() {
    let mut reg = ConnRegistry::new();
    let id = reg.allocate(
        7,
        ConnState {
            status: 200,
            response_body: vec![],
            response_headers: vec![],
            body_consumed: false,
            truncated: false,
        },
    );
    assert!(reg.get(id).is_some());
    reg.remove(id);
    assert!(reg.get(id).is_none());
    assert!(reg.by_owner.is_empty(), "removal unlinks the owner index");
}

#[test]
fn test_huc_init_sets_defaults() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let url_obj = ctx.alloc_object(cratonvm_types::ClassId::new(0), 1);
    let url_str = ctx.create_string("http://example.com/");
    ctx.set_field(url_obj, 0, Value::Object(Some(url_str)));
    let _ = huc_init(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(url_obj))],
    );
    assert_eq!(ctx.get_field(this, HUC_CONN_ID), Value::Int(-1));
    assert_eq!(ctx.get_field(this, HUC_DO_INPUT), Value::Int(1));
    assert_eq!(ctx.get_field(this, HUC_DO_OUTPUT), Value::Int(0));
}

#[test]
fn test_huc_set_request_method_normalizes() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
    let post = ctx.create_string("post");
    let _ = huc_set_request_method(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(post))],
    );
    let m = match ctx.get_field(this, HUC_METHOD) {
        Value::Object(Some(s)) => ctx.read_string(s).unwrap(),
        _ => panic!("expected method string"),
    };
    assert_eq!(m, "POST");
}

#[test]
fn test_huc_set_request_method_rejects_bogus() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
    let bad = ctx.create_string("FROBNICATE");
    let result = huc_set_request_method(
        &mut ctx,
        &[Value::Object(Some(this)), Value::Object(Some(bad))],
    );
    assert!(result.is_err());
}

#[test]
fn test_huc_disconnect_clears_state() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
    let _ = huc_disconnect(&mut ctx, &[Value::Object(Some(this))]);
    assert_eq!(ctx.get_field(this, HUC_CONN_ID), Value::Int(-1));
    assert_eq!(ctx.get_field(this, HUC_DISCONNECTED), Value::Int(1));
}

#[test]
fn test_huc_get_response_message_known_codes() {
    // Build a fake state and run get_response_message via the pipeline.
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
    // Cannot run ensure_connected without a real network — just exercise
    // the code-name table directly.
    let id = CONN_REGISTRY.with(ctx.vm_identity(), |reg| {
        reg.allocate(
            0,
            ConnState {
                status: 404,
                response_body: vec![],
                response_headers: vec![],
                body_consumed: false,
                truncated: false,
            },
        )
    });
    ctx.set_field(this, HUC_CONN_ID, Value::Int(id));
    ctx.set_field(this, HUC_CONNECTED, Value::Int(1));
    let res = huc_get_response_message(&mut ctx, &[Value::Object(Some(this))]).unwrap();
    let s = match res {
        Some(Value::Object(Some(o))) => ctx.read_string(o).unwrap(),
        _ => panic!("expected string"),
    };
    assert_eq!(s, "Not Found");
}

#[test]
fn test_huc_set_connect_timeout_negative_rejected() {
    let mut ctx = crate::test_utils::MockNativeContext::new();
    let this = ctx.alloc_object(cratonvm_types::ClassId::new(0), 12);
    let _ = huc_init(&mut ctx, &[Value::Object(Some(this))]);
    let res = huc_set_connect_timeout(&mut ctx, &[Value::Object(Some(this)), Value::Int(-1)]);
    assert!(res.is_err());
}

#[test]
fn test_anchor_pub_function_exists() {
    // Anchor-grep guard: enforce the exported API name keeps existing.
    let mut r = NativeMethodRegistry::new();
    register_http_url_connection_real(&mut r);
    assert!(!r.is_empty());
}

// -----------------------------------------------------------------
// Plain-HTTP keep-alive pool
// -----------------------------------------------------------------

#[test]
fn test_is_poolable_response_content_length() {
    let headers = vec![("Content-Length".to_string(), "5".to_string())];
    assert!(is_poolable_response(200, false, &headers));
}

#[test]
fn test_is_poolable_response_chunked_excluded() {
    let headers = vec![("Transfer-Encoding".to_string(), "chunked".to_string())];
    assert!(!is_poolable_response(200, false, &headers));
}

#[test]
fn test_is_poolable_response_connection_close_excluded() {
    let headers = vec![
        ("Content-Length".to_string(), "5".to_string()),
        ("Connection".to_string(), "close".to_string()),
    ];
    assert!(!is_poolable_response(200, false, &headers));
}

#[test]
fn test_is_poolable_response_bodiless_without_content_length() {
    // HEAD / 204 / 304 / 1xx carry no body and no Content-Length, but the
    // framing is still unambiguous — poolable.
    assert!(is_poolable_response(200, true, &[]));
    assert!(is_poolable_response(204, false, &[]));
    assert!(is_poolable_response(304, false, &[]));
    assert!(is_poolable_response(100, false, &[]));
}

#[test]
fn test_is_poolable_response_ambiguous_framing_excluded() {
    // A normal 200 with neither Content-Length nor chunked has no
    // reliable end-of-body marker other than connection close — not safe
    // to hand back to the pool.
    assert!(!is_poolable_response(200, false, &[]));
}

/// A loopback `TcpStream` pair for exercising `pool_take`/`pool_put`
/// against real sockets (the pool stores `TcpStream` directly, not a
/// generic `Read`, so a slice-backed fake won't do here).
fn loopback_pair() -> (TcpStream, TcpStream) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = TcpStream::connect(addr).unwrap();
    let (server, _) = listener.accept().unwrap();
    (client, server)
}

#[test]
fn test_pool_put_then_take_roundtrip() {
    let (client, _server) = loopback_pair();
    let host = format!("test-roundtrip-{}", client.local_addr().unwrap().port());
    let port = 1;
    pool_put(&host, port, client);
    assert!(pool_take(&host, port).is_some());
    // The bucket is empty now — a second take is a plain miss.
    assert!(pool_take(&host, port).is_none());
}

#[test]
fn test_pool_take_evicts_closed_peer() {
    let (client, server) = loopback_pair();
    let host = format!("test-closed-peer-{}", client.local_addr().unwrap().port());
    let port = 1;
    pool_put(&host, port, client);
    drop(server); // peer close -> FIN visible to a peek on `client`
                  // Give the FIN a moment to actually land in the kernel buffer.
    std::thread::sleep(Duration::from_millis(50));
    assert!(pool_take(&host, port).is_none());
}

#[test]
fn test_pool_take_respects_idle_window() {
    let (client, _server) = loopback_pair();
    let host = format!("test-idle-window-{}", client.local_addr().unwrap().port());
    let port = 1;
    if let Ok(mut guard) = conn_pool().lock() {
        guard
            .entry((host.clone(), port))
            .or_default()
            .push(PooledConn {
                stream: client,
                returned_at: Instant::now() - POOL_IDLE_WINDOW - Duration::from_millis(1),
            });
    }
    assert!(pool_take(&host, port).is_none());
}

#[test]
fn test_pool_put_caps_bucket_at_max_per_key() {
    let (first, _first_server) = loopback_pair();
    let host = format!("test-cap-{}", first.local_addr().unwrap().port());
    let port = 2;
    pool_put(&host, port, first);
    for _ in 0..(POOL_MAX_PER_KEY + 1) {
        let (client, _server) = loopback_pair();
        pool_put(&host, port, client);
    }
    let len = conn_pool()
        .lock()
        .unwrap()
        .get(&(host, port))
        .map(|b| b.len())
        .unwrap_or(0);
    assert_eq!(len, POOL_MAX_PER_KEY);
}

// -----------------------------------------------------------------------
// G31 — the per-connection `HostnameVerifier` that was never asked
//
// Rows measured 2026-08-17 against HotSpot 25.0.3+9-LTS over a live
// loopback TLS 1.3 handshake (`scratchpad/g31/HvFamily.java`,
// `HvCase.java`, `HvLambda.java`), one case per process.
// -----------------------------------------------------------------------

/// THE REGRESSION GUARD. `None` means "a verifier object exists whose class
/// this VM could not name" by the time it reaches this predicate — the
/// caller has already dealt with "no verifier at all" — and a lambda is
/// exactly that. Reading it as "the JDK default is installed" refused every
/// connection whose application verifier was written as a lambda.
///
/// MEASURED, same connection shape, same request, HotSpot vs CratonVM:
/// a named-class verifier was called on both (`calls=1`); a lambda was
/// called on HotSpot and NOT on CratonVM (`calls=0`).
#[test]
fn an_unnameable_verifier_is_not_a_default_stand_in() {
    assert!(
        !is_default_hostname_verifier(None),
        "a verifier whose class name could not be resolved is an APPLICATION \
         verifier (a lambda), not a JDK default — MEASURED: HotSpot calls it"
    );
}

/// The two entries that are genuinely "nothing application-specific is
/// installed", and a sample of the shapes that are not. Both survivors are
/// named by classes this VM always resolves, which is why the predicate can
/// afford to be a name match at all.
#[test]
fn the_two_default_stand_ins_are_the_only_ones() {
    assert!(is_default_hostname_verifier(Some(
        "javax/net/ssl/HttpsURLConnection$DefaultHostnameVerifier"
    )));
    assert!(is_default_hostname_verifier(Some(
        "javax/net/ssl/HostnameVerifier"
    )));
    // MEASURED: `HvFamily$Rec` is consulted on both VMs.
    assert!(!is_default_hostname_verifier(Some("HvFamily$Rec")));
    // A third-party permissive verifier must still be consulted.
    assert!(!is_default_hostname_verifier(Some(
        "org/apache/http/conn/ssl/NoopHostnameVerifier"
    )));
}

/// A verifier that DECLINED and a certificate that never matched are two
/// different facts, and HotSpot reports them with two different exception
/// classes and two different sentences. The messages are transcribed, so
/// they are asserted character for character.
#[test]
fn a_declined_verifier_and_an_unmatched_certificate_read_differently() {
    let declined = huc_verifier_declined_message("127.0.0.1");
    // MEASURED: java.io.IOException | Wrong HTTPS hostname: should be <127.0.0.1>
    assert_eq!(
        declined.trim_start_matches(TLS_HOSTNAME_REFUSED_SENTINEL),
        "Wrong HTTPS hostname: should be <127.0.0.1>"
    );
    assert!(
        declined.starts_with(TLS_HOSTNAME_REFUSED_SENTINEL),
        "the declined exit must route to the plain-IOException sentinel, not the \
         SSLPeerUnverifiedException one"
    );
    let unmatched = huc_unverified_peer_message("127.0.0.1");
    assert!(unmatched.starts_with(TLS_PEER_UNVERIFIED_SENTINEL));
    assert_ne!(
        declined.trim_start_matches(TLS_HOSTNAME_REFUSED_SENTINEL),
        unmatched.trim_start_matches(TLS_PEER_UNVERIFIED_SENTINEL),
        "MEASURED: HotSpot gives these two exits different messages"
    );
}

/// The two sentinels must stay distinguishable by prefix, or the arm that
/// picks the exception class would match the wrong one. They share no
/// prefix relation in either direction.
#[test]
fn the_tls_sentinels_do_not_shadow_each_other() {
    for (a, b) in [
        (TLS_HOSTNAME_REFUSED_SENTINEL, TLS_PEER_UNVERIFIED_SENTINEL),
        (
            TLS_HOSTNAME_REFUSED_SENTINEL,
            TLS_HANDSHAKE_FAILURE_SENTINEL,
        ),
        (TLS_HOSTNAME_REFUSED_SENTINEL, CONNECT_REFUSED_SENTINEL),
    ] {
        assert!(!a.starts_with(b), "{a} must not start with {b}");
        assert!(!b.starts_with(a), "{b} must not start with {a}");
    }
}

/// Regression test for `rustls::Stream::prepare_read`: when a TLS 1.2 server
/// sends a `HelloRequest`, rustls queues a `NoRenegotiation` warning alert and
/// `complete_io` returns `(0, wrlen)` with `wrlen > 0`. `prepare_read` must not
/// break out of its loop on `rd == 0` when `wr > 0`, which previously caused
/// `Reader::read` to return `WouldBlock` (`SocketTimeoutException: Read timed
/// out`) on a blocking socket.
#[cfg(unix)]
#[test]
fn rustls_stream_does_not_report_would_block_on_hello_request() {
    use openssl::asn1::Asn1Time;
    use openssl::bn::{BigNum, MsbOption};
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::rsa::Rsa;
    use openssl::ssl::{SslAcceptor, SslMethod, SslVersion};
    use openssl::x509::{X509Builder, X509NameBuilder};
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
    use std::io::{Read as _, Write as _};

    #[derive(Debug)]
    struct AcceptAll;
    impl ServerCertVerifier for AcceptAll {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn verify_tls13_signature(
            &self,
            _: &[u8],
            _: &CertificateDer<'_>,
            _: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            Ok(HandshakeSignatureValid::assertion())
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            vec![
                SignatureScheme::RSA_PKCS1_SHA256,
                SignatureScheme::RSA_PSS_SHA256,
                SignatureScheme::ECDSA_NISTP256_SHA256,
            ]
        }
    }

    let rsa = Rsa::generate(2048).unwrap();
    let pkey = PKey::from_rsa(rsa).unwrap();
    let mut name = X509NameBuilder::new().unwrap();
    name.append_entry_by_text("CN", "localhost").unwrap();
    let name = name.build();
    let mut builder = X509Builder::new().unwrap();
    builder.set_version(2).unwrap();
    let mut serial = BigNum::new().unwrap();
    serial.rand(64, MsbOption::MAYBE_ZERO, false).unwrap();
    let serial = serial.to_asn1_integer().unwrap();
    builder.set_serial_number(&serial).unwrap();
    builder.set_subject_name(&name).unwrap();
    builder.set_issuer_name(&name).unwrap();
    builder.set_pubkey(&pkey).unwrap();
    builder
        .set_not_before(&Asn1Time::days_from_now(0).unwrap())
        .unwrap();
    builder
        .set_not_after(&Asn1Time::days_from_now(30).unwrap())
        .unwrap();
    builder.sign(&pkey, MessageDigest::sha256()).unwrap();
    let cert = builder.build();

    let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls_server()).unwrap();
    acceptor.set_min_proto_version(Some(SslVersion::TLS1_2)).unwrap();
    acceptor.set_max_proto_version(Some(SslVersion::TLS1_2)).unwrap();
    acceptor.set_private_key(&pkey).unwrap();
    acceptor.set_certificate(&cert).unwrap();
    let acceptor = acceptor.build();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (tcp, _) = listener.accept().unwrap();
        let mut ssl = acceptor.accept(tcp).unwrap();
        let mut req = [0u8; 32];
        let _ = ssl.read(&mut req);
        extern "C" {
            fn SSL_renegotiate(s: *mut std::ffi::c_void) -> std::ffi::c_int;
            fn SSL_do_handshake(s: *mut std::ffi::c_void) -> std::ffi::c_int;
        }
        use foreign_types_shared::ForeignTypeRef as _;
        unsafe {
            let ptr = ssl.ssl().as_ptr() as *mut std::ffi::c_void;
            SSL_renegotiate(ptr);
            SSL_do_handshake(ptr);
        }
        let _ = ssl.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK");
    });

    let cfg = Arc::new(
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAll))
            .with_no_client_auth(),
    );
    let tcp = TcpStream::connect(addr).unwrap();
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let conn = ClientConnection::new(cfg, ServerName::try_from("localhost").unwrap()).unwrap();
    let mut stream = StreamOwned::new(conn, tcp);
    stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
    stream.flush().unwrap();

    let mut buf = [0u8; 64];
    match stream.read(&mut buf) {
        Ok(n) => assert!(n > 0),
        Err(err) => assert_ne!(
            err.kind(),
            std::io::ErrorKind::WouldBlock,
            "HelloRequest must not surface as WouldBlock on a blocking socket"
        ),
    }
    assert!(stream.conn.renegotiation_requested);
    let _ = server.join();
}
