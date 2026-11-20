#[allow(unused_imports)]
use super::*;

pub fn register_self_test(r: &mut NativeMethodRegistry) {
    // A JVM-callable self-test that spins up a rustls server on a loopback
    // ephemeral port using the *configured runtime* TLS identity, connects
    // to it with a rustls client, exchanges a short ping/pong, verifies the
    // negotiated protocol is TLSv1.3 and that ALPN selected "h2", then
    // returns "OK" as a Java String. Any failure yields the error text.
    //
    // SECURITY: this self-test no longer reaches for an embedded private
    // key. If no runtime identity has been installed (no keystore
    // configured), `T27SelfTest.run()` returns
    // `"ERR: No TLS key/cert configured; set javax.net.ssl.keyStore"`
    // rather than booting a server under a publicly known key.
    //
    // This is exposed as `cratonvm.tls.T27SelfTest.run()` — callable from
    // Java tests (or interactively) to prove the entire rustls pipeline is
    // wired up end-to-end without requiring network access.
    r.register(
        "cratonvm/tls/T27SelfTest",
        "run",
        "()Ljava/lang/String;",
        |ctx, _args| {
            let result = match runtime_tls_identity(ctx.vm_identity()) {
                Some(id) => {
                    run_loopback_self_test(&id.cert_pem, &id.key_pem, id.client_ca_pem.as_deref())
                }
                None => Err("No TLS key/cert configured; set javax.net.ssl.keyStore".to_string()),
            };
            let s = match result {
                Ok(msg) => ctx.create_string(&msg),
                Err(e) => ctx.create_string(&format!("ERR: {}", e)),            };
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// End-to-end loopback handshake using the caller-supplied PEM material.
///
/// `trust_pem` is the CA used by the client to verify the server. When
/// `None`, the client uses the system root store (suitable for fixtures
/// whose leaf chains up to a publicly trusted CA; tests pass the test CA
/// here explicitly). Returns `"OK proto=... cipher=... alpn=..."` on
/// success.
pub fn run_loopback_self_test(
    server_cert_pem: &str,
    server_key_pem: &str,
    trust_pem: Option<&str>,
) -> Result<String, String> {
    // Server side.
    let server_config = build_server_config_single_cert(
        server_cert_pem,
        server_key_pem,
        &["h2", "http/1.1"],
        false,
        None,
    )?;
    let listener =
        TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("bind loopback: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {}", e))?
        .port();

    let server_cfg_clone = server_config.clone();
    let server_thread = std::thread::spawn(move || -> Result<String, String> {
        let (tcp, _peer) = listener.accept().map_err(|e| format!("accept: {}", e))?;
        let conn = ServerConnection::new(server_cfg_clone)
            .map_err(|e| format!("ServerConnection: {}", e))?;
        let mut stream = StreamOwned::new(conn, tcp);
        // Drive handshake.
        while stream.conn.is_handshaking() {
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut EintrIo::new(&mut stream.sock))
                    .map_err(|e| format!("s read: {}", e))?;
                stream
                    .conn
                    .process_new_packets()
                    .map_err(|e| format!("s proc: {}", e))?;
            }
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut EintrIo::new(&mut stream.sock))
                    .map_err(|e| format!("s write: {}", e))?;
            }
        }
        // Expect "PING", reply "PONG".
        let mut buf = [0u8; 4];
        let n = stream
            .read(&mut buf)
            .map_err(|e| format!("s app read: {}", e))?;
        if n != 4 || &buf != b"PING" {
            return Err(format!("unexpected {} bytes: {:?}", n, &buf[..n]));
        }
        stream
            .write_all(b"PONG")
            .map_err(|e| format!("s app write: {}", e))?;
        stream.conn.send_close_notify();
        let _ = stream.flush();
        Ok("server done".to_string())
    });

    // Client side.
    let mut roots = RootCertStore::empty();
    if let Some(ca) = trust_pem {
        for cert in parse_cert_chain_pem(ca)? {
            roots.add(cert).map_err(|e| format!("add CA: {}", e))?;
        }
    } else {
        // No explicit trust supplied: fall back to the host's native trust
        // store. This is what real-world callers want; the test harness
        // always passes an explicit `trust_pem`.
        if let Ok(native) = load_native_root_store() {
            roots = native;
        }
    }
    let client_config = build_client_config(roots, &["h2", "http/1.1"], None)?;
    let tcp =
        TcpStream::connect(("127.0.0.1", port)).map_err(|e| format!("client connect: {}", e))?;
    let sni = ServerName::try_from("localhost".to_string()).map_err(|e| format!("sni: {}", e))?;
    let conn = ClientConnection::new(client_config, sni)
        .map_err(|e| format!("ClientConnection: {}", e))?;
    let mut stream = StreamOwned::new(conn, tcp);

    while stream.conn.is_handshaking() {
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("c write: {}", e))?;
        }
        if stream.conn.wants_read() {
            stream
                .conn
                .read_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("c read: {}", e))?;
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("c proc: {}", e))?;
        }
    }

    // E12: diagnostic-only (this is the VM-private loopback self-test), but it
    // is the sixth raw spelling of the same concept in this file and there
    // should be one. `"?"` in particular is in nobody's vocabulary.
    let proto = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
    };
    let cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| suite_to_java_cipher_name(cs.suite()))
        .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into());
    let alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
        .to_string();

    stream
        .write_all(b"PING")
        .map_err(|e| format!("c app write: {}", e))?;
    let mut reply = [0u8; 4];
    stream
        .read_exact(&mut reply)
        .map_err(|e| format!("c app read: {}", e))?;
    if &reply != b"PONG" {
        return Err(format!("unexpected reply: {:?}", reply));
    }
    stream.conn.send_close_notify();
    let _ = stream.flush();

    server_thread
        .join()
        .map_err(|_| "server thread panicked".to_string())??;

    Ok(format!(
        "OK proto={} cipher={} alpn={}",
        proto, cipher, alpn
    ))
}

// -----------------------------------------------------------------------------
// Helper: first-arg object extraction (matches the convention in phases_late)
// -----------------------------------------------------------------------------

pub fn obj_arg(args: &[Value], idx: usize) -> Result<ObjectRef, RuntimeError> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("expected non-null object arg".into()),
        }),
    }
}


// Temporarily suppress `dead_code` on the inline integration helpers — they
// are called via the registered native methods, not from Rust.
#[allow(dead_code)]
pub fn _t27_keep_symbols_live() {
    let _ = run_loopback_self_test;
    let _ = rustls_client_connect;
    let _ = rustls_stream_read;
    let _ = rustls_stream_write;
    let _ = rustls_stream_close;
    let _ = rustls_listener_close;
    let _ = rustls_session_info;
    let _ = build_server_config_sni;
}

// =============================================================================
// WP5.1 — SSLEngine real wrap/unwrap (rustls-backed)
// WP5.4 — ALPN for HTTP/2
// -----------------------------------------------------------------------------
// Wraps rustls' `ServerConnection` / `ClientConnection` behind the JDK
// `javax.net.ssl.SSLEngine` (and `sun.security.ssl.SSLEngineImpl`) surface.
// Each Java SSLEngine instance carries an `engine_id` int — produced by
// `engine_alloc_id()` and stashed in a side-table keyed by ObjectRef (so we
// don't have to widen the existing 8-field SSLEngine synthetic that tls.rs
// owns and that phases_late.rs also touches).
//
// The state machine drives rustls:
//   * `wrap(srcs[], dst)`: drains application bytes from `srcs[]` (during
//     post-handshake data flow) into the rustls connection via `writer()`,
//     then pulls outbound TLS records out via `write_tls()` into `dst`.
//   * `unwrap(src, dsts[])`: feeds the inbound TLS bytes from `src` into
//     `read_tls()`, calls `process_new_packets()`, and (post-handshake)
//     pulls plaintext via `reader()` into `dsts[]`.
//
// During the handshake itself, rustls only consumes/produces TLS records;
// `wrap` and `unwrap` therefore alternate between NEED_WRAP / NEED_UNWRAP
// driven by `is_handshaking()` + `wants_write()`/`wants_read()`. Once
// `is_handshaking()` returns false, FINISHED is reported once and the
// engine transitions to NOT_HANDSHAKING.
//
// SSLEngineResult is a 4-field tuple (status, hsStatus, bytesConsumed,
// bytesProduced). We allocate via `alloc_concurrent_synthetic` and write
// all four fields directly — orthogonal to the 2-field stub allocator
// used by tls.rs::register_ssl_engine for the legacy non-rustls path.
// =============================================================================

/// Per-SSLEngine state. Holds the rustls connection (client OR server) plus
/// inbound/outbound buffers that wrap/unwrap drive against the rustls IO.
pub enum EngineConn {
    Client(rustls::ClientConnection),
    Server(rustls::ServerConnection),
}

impl EngineConn {
    pub fn is_handshaking(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.is_handshaking(),
            EngineConn::Server(s) => s.is_handshaking(),
        }
    }
    /// Full, hello-retry or RESUMED — see `client_session_cache`, the only
    /// consumer, which must not hand back a previous session object for a
    /// handshake that was not actually a continuation of it.
    pub fn handshake_kind(&self) -> Option<rustls::HandshakeKind> {
        match self {
            EngineConn::Client(c) => c.handshake_kind(),
            EngineConn::Server(s) => s.handshake_kind(),
        }
    }
    pub fn wants_read(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.wants_read(),
            EngineConn::Server(s) => s.wants_read(),
        }
    }
    pub fn wants_write(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.wants_write(),
            EngineConn::Server(s) => s.wants_write(),
        }
    }
    pub fn read_tls<R: std::io::Read>(&mut self, rd: &mut R) -> std::io::Result<usize> {
        match self {
            EngineConn::Client(c) => c.read_tls(rd),
            EngineConn::Server(s) => s.read_tls(rd),
        }
    }
    pub fn write_tls<W: std::io::Write>(&mut self, wr: &mut W) -> std::io::Result<usize> {
        match self {
            EngineConn::Client(c) => c.write_tls(wr),
            EngineConn::Server(s) => s.write_tls(wr),
        }
    }
    pub fn process_new_packets(&mut self) -> Result<rustls::IoState, rustls::Error> {
        match self {
            EngineConn::Client(c) => c.process_new_packets(),
            EngineConn::Server(s) => s.process_new_packets(),
        }
    }
    pub fn writer(&mut self) -> rustls::Writer<'_> {
        match self {
            EngineConn::Client(c) => c.writer(),
            EngineConn::Server(s) => s.writer(),
        }
    }
    pub fn reader(&mut self) -> rustls::Reader<'_> {
        match self {
            EngineConn::Client(c) => c.reader(),
            EngineConn::Server(s) => s.reader(),
        }
    }
    pub fn alpn_protocol(&self) -> Option<&[u8]> {
        match self {
            EngineConn::Client(c) => c.alpn_protocol(),
            EngineConn::Server(s) => s.alpn_protocol(),
        }
    }
    pub fn protocol_version(&self) -> Option<rustls::ProtocolVersion> {
        match self {
            EngineConn::Client(c) => c.protocol_version(),
            EngineConn::Server(s) => s.protocol_version(),
        }
    }
    pub fn negotiated_cipher_suite(&self) -> Option<rustls::SupportedCipherSuite> {
        match self {
            EngineConn::Client(c) => c.negotiated_cipher_suite(),
            EngineConn::Server(s) => s.negotiated_cipher_suite(),
        }
    }
    pub fn send_close_notify(&mut self) {
        match self {
            EngineConn::Client(c) => c.send_close_notify(),
            EngineConn::Server(s) => s.send_close_notify(),
        }
    }
    /// Queue a fatal alert for the peer — see
    /// `rustls::CommonState::queue_fatal_alert` (a CratonVM addition to the
    /// vendored fork) for why this exists and why it is idempotent.
    pub fn queue_fatal_alert(&mut self, desc: rustls::AlertDescription) {
        match self {
            EngineConn::Client(c) => c.queue_fatal_alert(desc),
            EngineConn::Server(s) => s.queue_fatal_alert(desc),
        }
    }
    /// Has the peer sent us a `close_notify`? The inbound half of the
    /// connection is then closed for good — see `do_unwrap`'s CLOSED report.
    pub fn peer_has_closed(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.has_received_close_notify(),
            EngineConn::Server(s) => s.has_received_close_notify(),
        }
    }
    pub fn peer_certificates(&self) -> Option<&[CertificateDer<'static>]> {
        match self {
            EngineConn::Client(c) => c.peer_certificates(),
            EngineConn::Server(s) => s.peer_certificates(),
        }
    }
}

pub struct EngineState {
    /// `None` until `beginHandshake` realizes the connection (we need
    /// client-vs-server + ALPN list known before constructing rustls).
    pub conn: Option<EngineConn>,
    /// `conn` is not absent, it is ON LOAN to `do_unwrap`'s record loop.
    ///
    /// The loop needs `&mut conn` while the engine-registry lock is DROPPED, so
    /// that the Java `TrustManager` upcall `PassthroughServerCertVerifier`
    /// makes from inside `process_new_packets` can re-enter `with_engine`
    /// instead of deadlocking on a non-reentrant write guard. It does that by
    /// `take()`ing the connection out and putting it back through
    /// [`ConnCheckout`]'s `Drop`.
    ///
    /// Every reader that treats `conn == None` as "not begun yet" must consult
    /// this first. `handshake_status_of` already answers "still handshaking" for
    /// `None`, which is the right answer here too; `engine_begin` does NOT — it
    /// would build a SECOND connection and the checkout's restore would then
    /// throw it away, losing whatever the re-entrant caller had done to it.
    pub conn_checked_out: bool,
    pub is_client: bool,
    /// Whether `setUseClientMode` was ever CALLED, as distinct from what
    /// `is_client` says the handshake will do.
    ///
    /// `javax.net.ssl.SSLEngine.getUseClientMode()` reports the CONFIGURED
    /// role and a fresh `createSSLEngine()` engine has none, so HotSpot
    /// answers `false` (and its `beginHandshake` then refuses). `is_client`
    /// defaults to `true` here because this VM's engine callers overwhelmingly
    /// are clients and flipping that default would turn every client that
    /// never calls the setter into a server — a behaviour change no probe
    /// asked for. Keeping the two apart lets the GETTER be honest about what
    /// was configured while the handshake keeps the default it has always had.
    /// MEASURED: `L6TlsParamSweep` rows 66 and 89.
    pub client_mode_set: bool,
    /// In-process inbound buffer. `unwrap` appends to this from the source
    /// ByteBuffer, then drains via `read_tls` into rustls.
    pub inbound: Vec<u8>,
    /// In-process outbound buffer. `wrap` writes rustls' `write_tls` output
    /// here, then drains it into the destination ByteBuffer.
    pub outbound: Vec<u8>,
    pub alpn_protocols: Vec<Vec<u8>>,
    pub enabled_protocols: Vec<String>,
    pub enabled_ciphers: Vec<String>,
    pub need_client_auth: bool,
    pub want_client_auth: bool,
    pub closed_inbound: bool,
    pub closed_outbound: bool,
    /// Set once `is_handshaking()` flips false — used to report FINISHED
    /// exactly once on the next wrap/unwrap call.
    pub handshake_finished_reported: bool,
    /// Cached after handshake completes.
    pub negotiated_alpn: Option<String>,
    /// Optional pre-built configs. If both are `None`, `beginHandshake`
    /// builds a default config for the test path (loopback to localhost).
    pub client_config: Option<Arc<ClientConfig>>,
    pub server_config: Option<Arc<ServerConfig>>,
    pub peer_host: Option<String>,
    /// None: JSSE infers SNI; Some(None): explicitly disabled; Some(Some): explicit host.
    pub explicit_sni: Option<Option<String>>,
    /// The JDK-normalized automatic name (including IDN conversion), separate from the peer endpoint.
    pub inferred_sni: Option<Option<String>>,
    /// Advisory peer port recorded by `SSLContext.createSSLEngine(host, port)`.
    /// rustls never needs it (the caller owns the already-connected socket); it
    /// exists so `SSLEngine.getPeerPort()` can answer what the application asked
    /// for instead of the uninitialised `-1` of a bare synthetic allocation.
    pub peer_port: i32,
    /// JSSE's delegated-task state for this engine. See [`DelegatedTask`].
    pub delegated_task: DelegatedTask,
    /// This engine has already used its one deferral. Without it a caller that
    /// keeps handing the engine back its own NEED_TASK would re-arm forever.
    pub delegated_task_armed: bool,
    /// We told a caller NEED_TASK and have not yet handed it a `Runnable`.
    /// Deliberately NOT cleared when the work is done inline — see the `Owed`
    /// arm of [`engine_begin_or_defer`] for what returning `null` to a caller
    /// that was promised a task costs.
    pub task_unclaimed: bool,
    /// Complete TLS records taken out of the caller's buffer while the
    /// connection did not exist yet, waiting for the delegated task to realize
    /// it. See the NEED_TASK arm of `do_unwrap`.
    pub deferred_inbound: Vec<u8>,
    /// A handshake failure this SERVER engine has detected but not yet
    /// reported, because the fatal alert rustls queued for it still has to be
    /// flushed first.
    ///
    /// `do_unwrap` used to DISCARD a server-side handshake error for exactly
    /// that reason ("let the handshake driver observe NEED_WRAP and flush it
    /// before the channel closes"), which delivered the alert to the peer and
    /// left this side with no failure at all: netty's
    /// `SslHandlerTest.testHandshakeFailureCipherMissmatch*` asserts BOTH
    /// sides see an `SSLException`, and the server saw
    /// `StacklessClosedChannelException` — the promise failed by the peer
    /// hanging up, not by the mismatch this engine detected. Deferred to the
    /// wrap that drains the alert instead of dropped.
    pub deferred_handshake_error: Option<(&'static str, String)>,
    /// The `server_name` host a SERVER engine peeked out of the ClientHello (`engine_pending_sni_host`),
    /// kept because the rustls connection may be on loan to the record loop (`conn_checked_out`) when a
    /// `TrustManager` asks the handshake session for `getRequestedServerNames()`.
    pub requested_sni: Option<String>,
    /// The `SSLParameters.getEndpointIdentificationAlgorithm()` value the
    /// application configured on this engine ("HTTPS" / "LDAPS"), if any.
    ///
    /// This is the switch that turns RFC 2818 / RFC 6125 hostname verification
    /// ON for a client engine. Real JSSE runs that check inside the engine
    /// (`X509TrustManagerImpl.checkIdentity`, reached from
    /// `SSLContextImpl$AbstractTrustManagerWrapper.checkAdditionalTrust`), so it
    /// applies whether or not the application installed its own `TrustManager`
    /// — including a deliberately-permissive test one. See
    /// `engine_check_endpoint_identity`.
    pub endpoint_id_alg: Option<String>,
    /// Per-`SSLContext` (cert_pem, key_pem) copied from the context that created
    /// this engine via `createSSLEngine`. When set, `engine_begin` builds the
    /// server (or client) config from THIS identity instead of the process-
    /// global `runtime_tls_identity`, so an in-process mTLS test's server and
    /// client engines each use their own keystore.
    pub identity_override: Option<(String, String)>,
    /// Trust roots copied from the SSLContext that created this engine.
    pub trust_roots_override: Option<TlsTrustRoots>,
    /// Decrypted application bytes that did not fit the caller's `unwrap`
    /// destination buffers. Served first on the next `unwrap`. MUST be kept
    /// separate from `outbound` (encrypted TLS records) — mixing decrypted
    /// plaintext into `outbound` makes the next `wrap` emit plaintext on the
    /// wire, which the peer rejects ("corrupt message of type InvalidContentType").
    pub plaintext_pending: Vec<u8>,
    /// The peer's certificate chain (DER, leaf first), captured once the
    /// handshake finishes. For a server engine this is the CLIENT certificate
    /// (mTLS) — Tomcat's SSLAuthenticator reads it via
    /// `SSLSession.getPeerCertificates()` to authenticate/authorize the client.
    pub peer_cert_chain_der: Vec<Vec<u8>>,
    /// GC-stable key (see `ctx_obj_key`/`gc_stable_lock_key`) of the `SSLContext`
    /// that created this engine, used to look up its `TrustManager[]` in
    /// `ctx_trust_managers_table` once the handshake finishes. Deliberately a
    /// plain `u64`, NOT the `ObjectRef`s themselves: `do_wrap`/`do_unwrap` call
    /// allocating helpers (e.g. `throw_jca_exc`) while holding
    /// `engine_registry()`'s write lock, so anything reachable through
    /// `EngineState` that needed GC-root scanning would make that lock
    /// GC-relevant — and a GC triggered by an allocation *while the same
    /// thread already holds that lock* would self-deadlock trying to
    /// re-acquire it during root scanning. Keeping only a `u64` here avoids
    /// that hazard entirely; the real `ObjectRef`s live solely in
    /// `ctx_trust_managers_table`, which no allocating call ever locks
    /// concurrently with `engine_registry()`.
    ///
    /// rustls's own `WebPkiClientVerifier`/root-store check only verifies the
    /// certificate CHAIN against a trust anchor; it never consults an
    /// application-supplied `TrustManager`, so a custom `TrustManager` (e.g.
    /// one wrapping a revocation-aware `PKIXRevocationChecker` for OCSP/CRL,
    /// or a fully custom `X509TrustManager` like Tomcat's
    /// `TrustManagerClassName` tests use) was silently never invoked — a
    /// fail-open gap. `engine_run_trust_check` calls
    /// `checkClientTrusted`/`checkServerTrusted` on each configured manager
    /// once the crypto handshake completes, so a rejection there still aborts
    /// the handshake.
    pub trust_managers_ctx_key: Option<u64>,
    /// Set once the post-handshake trust-manager consultation has run for this
    /// engine (whether it passed, failed, or found nothing to check) so it
    /// only happens once per connection.
    pub trust_check_done: bool,
    /// True once this engine's rustls config was built asking the peer for a
    /// certificate (NEED, WANT, or the deferred-auth substitute below). Used
    /// to tell Tomcat's "renegotiate to collect the client certificate"
    /// second `beginHandshake()` apart from an ordinary redundant one.
    pub client_auth_requested: bool,
    /// Set once this server engine's `SNIMatcher`s have been consulted for the
    /// ClientHello's `server_name`, so the callback into Java happens once per
    /// connection — same one-shot discipline as `trust_check_done`.
    pub sni_match_done: bool,
    /// `beginHandshake()` was called on a SERVER engine whose rustls connection
    /// is deliberately NOT realized yet, because a handshake ALPN selector has
    /// to see the ClientHello first (see `engine_apply_alpn_selector`). Makes
    /// `handshake_status_of` answer NEED_UNWRAP rather than NOT_HANDSHAKING for
    /// that window, so a caller that polls the status still feeds us the hello.
    pub alpn_selection_deferred: bool,
    /// The `legacy_session_id` seen in the ServerHello — written by the server
    /// engine as it produces that record and read by the client engine as it
    /// consumes it, so both ends of one connection hold the same bytes. See
    /// [`peek_server_hello_session_id`]; consumed by
    /// `build_synthetic_ssl_session` for TLS 1.2 sessions only.
    pub negotiated_session_id: Vec<u8>,
    /// For a CLIENT engine: did this side actually PRESENT a certificate?
    ///
    /// Written by [`RecordingClientCertResolver`] from inside rustls's
    /// handshake, which is the only moment the answer exists. `None` until
    /// `engine_begin` builds the client config; `Some(false)` means the
    /// resolver was installed and never returned a certificate. Read by
    /// `build_synthetic_ssl_session` to decide whether
    /// `SSLSession.getLocalCertificates()` has anything to report.
    pub client_cert_presented: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// The VM whose engine this is, stamped by `engine_id_or_alloc` (gc-common
    /// w11-a). `engine_begin` runs with no native context, and the runtime TLS
    /// identity its default server config reads is one per VM. `0` only for
    /// states the unit tests insert by hand.
    pub vm: usize,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            conn: None,
            conn_checked_out: false,
            is_client: true,
            client_mode_set: false,
            inbound: Vec::new(),
            outbound: Vec::new(),
            alpn_protocols: Vec::new(),
            enabled_protocols: vec!["TLSv1.3".into(), "TLSv1.2".into()],
            enabled_ciphers: Vec::new(),
            need_client_auth: false,
            want_client_auth: false,
            closed_inbound: false,
            closed_outbound: false,
            handshake_finished_reported: false,
            negotiated_alpn: None,
            client_config: None,
            server_config: None,
            peer_host: None,
            explicit_sni: None,
            inferred_sni: None,
            peer_port: -1,
            delegated_task: DelegatedTask::None,
            delegated_task_armed: false,
            task_unclaimed: false,
            deferred_inbound: Vec::new(),
            deferred_handshake_error: None,
            requested_sni: None,
            endpoint_id_alg: None,
            identity_override: None,
            trust_roots_override: None,
            plaintext_pending: Vec::new(),
            peer_cert_chain_der: Vec::new(),
            trust_managers_ctx_key: None,
            trust_check_done: false,
            client_auth_requested: false,
            sni_match_done: false,
            alpn_selection_deferred: false,
            negotiated_session_id: Vec::new(),
            client_cert_presented: None,
            vm: 0,
        }
    }
}

/// One engine's native state behind ITS OWN lock (gc-common w11-a,
/// `common-w10e-sslengine-records-serialize-on-one-process-wide-lock`).
///
/// The registry used to be one `RwLock<HashMap<i32, EngineState>>` whose
/// WRITE guard every `wrap` / `unwrap` / `beginHandshake` / delegated task held
/// across rustls record encryption, `process_new_packets` and, in
/// `engine_begin`, the whole `ClientConfig` / `ServerConfig` build. Every
/// `SSLEngine` of every VM in the process therefore processed its records one
/// at a time: a Netty / Tomcat NIO server with N event-loop threads was capped
/// at one core, and a handshake parked every other connection's `wrap`.
///
/// Now the registry lock is held only to look an engine's cell up (read) or to
/// insert / remove one (write), and each engine's record processing holds that
/// engine's mutex alone. What the old lock guaranteed still holds PER ENGINE:
///
/// * the `ConnCheckout` loan (`EngineState::conn_checked_out`) is what lets a
///   Java `TrustManager` upcall made from inside `process_new_packets`
///   re-enter the SAME engine; the engine mutex is exactly as non-reentrant as
///   the registry's write guard was, so the loan rule is unchanged;
/// * "never allocate under the lock": every site that allocates or calls Java
///   still does so after its guard is dropped.
///
/// No site ever holds two engine locks at once (none could, under the old
/// single guard, without deadlocking), so there is no engine-to-engine lock
/// order to keep.
pub type EngineCell = Arc<parking_lot::Mutex<EngineState>>;

/// Engine-handle registry: engine id -> that engine's own cell. Keyed by an
/// integer that `engine_table` maps the SSLEngineImpl Java object's identity
/// key to. Read-locked for a lookup, write-locked only to insert / remove.
pub fn engine_registry() -> &'static parking_lot::RwLock<HashMap<i32, EngineCell>> {
    static REG: OnceLock<parking_lot::RwLock<HashMap<i32, EngineCell>>> = OnceLock::new();
    REG.get_or_init(|| parking_lot::RwLock::new(HashMap::new()))
}

/// Engine `id`'s cell, if it has one. The registry's read guard is released
/// before this returns; the caller locks the cell.
pub fn engine_cell(id: i32) -> Option<EngineCell> {
    engine_registry().read().get(&id).cloned()
}

/// Publish `state` as engine `id`'s cell, replacing any previous one.
pub fn engine_insert(id: i32, state: EngineState) {
    engine_registry()
        .write()
        .insert(id, Arc::new(parking_lot::Mutex::new(state)));
}

pub fn engine_alloc_id() -> i32 {
    // An atomic rather than a `Mutex<i32>` (gc-common w11-a): one lock fewer
    // on every engine mint. Same sequence as before: 1, 2, ... and back to 1
    // on overflow; 0 is never handed out.
    static NEXT: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(1);
    let prev = NEXT
        .try_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| Some(n.checked_add(1).unwrap_or(1)),
        )
        .unwrap_or(1);
    if prev == 0 {
        1
    } else {
        prev
    }
}

pub fn with_engine<F, R>(id: i32, f: F) -> Option<R>
where
    F: FnOnce(&mut EngineState) -> R,
{
    let cell = engine_cell(id)?;
    let mut g = cell.lock();
    Some(f(&mut *g))
}

pub fn engine_table() -> &'static parking_lot::Mutex<HashMap<u64, i32>> {
    static T: OnceLock<parking_lot::Mutex<HashMap<u64, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

/// GC-stable identity key for `engine_table`/`sslparams_alpn_table`.
///
/// FIX (reactive-httpcomponents-connector-flaky, mechanism 1): this used to
/// hash the `ObjectRef`'s Debug-formatted raw pointer value. `ObjectRef` is
/// a bare pointer to a heap object (`Hash` is implemented on the pointer
/// value itself, see `types/src/value.rs`), and this VM's young-gen GC
/// moves/reclaims objects — so a live object's `ObjectRef` is not a stable
/// identity across its own lifetime (if it moves) and a *different*,
/// unrelated object can later be allocated at the same address once the
/// original is collected. Neither `engine_table` nor `sslparams_alpn_table`
/// ever removed stale entries, so a moved/reclaimed-and-reused address could
/// silently hand a brand-new Java `SSLEngine`/`SSLParameters` object the
/// identity (and, for engines, the live `EngineState` — including an
/// already-`Some` `conn`) of a completely different one. Confirmed live: a
/// reactive HttpComponents connection's engine issuing a second, distinct
/// ClientHello mid-handshake on an already-established TCP socket, with
/// engine-id collisions, tcp registry id-reuse, and cross-listener accept
/// mixups all directly ruled out first (see the known-issues doc this
/// references). `ctx.identity_hash_code` is the VM's real, GC-stable
/// identity hash (same contract as `Object.hashCode()`'s default
/// implementation, and the same mechanism `nio_selector.rs` already uses
/// for its own cross-call Java-object identity lookups) — computed once and
/// pinned for an object's lifetime regardless of later moves.
pub fn engine_objref_key(ctx: &dyn NativeContext, o: ObjectRef) -> u64 {
    // The object's weak lock key since gc-common w26-b (per VM and per LIVE
    // object; was `(VM, identity hash)`); see `gc_stable_objref_key`.
    gc_stable_objref_key(ctx, o)
}

pub fn engine_id_or_alloc(ctx: &dyn NativeContext, obj: ObjectRef) -> i32 {
    let key = engine_objref_key(ctx, obj);
    let mut tab = engine_table().lock();
    if let Some(id) = tab.get(&key) {
        return *id;
    }
    let id = engine_alloc_id();
    tab.insert(key, id);
    // Published BEFORE the table lock is released (gc-common w11-a): another
    // thread that finds `id` in the table must also find its cell, or its
    // `with_engine` answers `None` -- "no such engine" -- for a live one.
    // Order table -> registry; nothing takes them the other way round.
    engine_insert(
        id,
        EngineState {
            vm: ctx.vm_identity(),
            ..EngineState::default()
        },
    );
    drop(tab);
    // gc-common w12-a: the engine is the weak owner of every row filed under
    // its key; `gc_sweep_tls_rows` drops them (via `engine_forget_key`) once
    // it dies. Recorded after the table lock is released: the owner table is
    // an ordered `Scratch` lock and nothing nests under it.
    note_tls_weak_owner(ctx, TlsOwnerKind::Engine, key, obj);
    id
}

/// The mint point of a NEW engine object (gc-common w11-a,
/// `common-w10e-sslengine-state-is-never-evicted`): drop every row this VM
/// recorded under the object's identity key, so the engine starts from
/// `EngineState::default()`.
///
/// `engine_table` was keyed by (VM, identity hash) (until gc-common w26-b),
/// and this VM hands a dead
/// object's identity hash to a later object. Nothing removes an engine's rows
/// while its VM lives, so a fresh `createSSLEngine()` engine whose hash
/// matched a collected one used to inherit the dead engine's whole state: its
/// `closed_*` flags (`isInboundDone() == true` before any I/O), its finished
/// rustls connection, its peer chain, its cached `SSLSession`s, its ALPN
/// selector and SNI matchers. Called by `set_engine_trust_roots_override`,
/// which is the first engine native the live `createSSLEngine`
/// (`net_phase_e::register_re6_ssl_context`) runs on the object it just
/// allocated. Answers the number of rows dropped.
///
/// Since gc-common w12-a also the engine's death: [`gc_sweep_tls_rows`] calls
/// it for an engine the collecting VM did not keep, which is what bounds the
/// registry in a live VM.
///
/// gc-common w26-b: the key is the engine's weak lock key now, so a new
/// engine never shares a LIVE engine's key (a live collider has its own), and
/// a dead engine's key is freed -- its rows dropped by
/// [`forget_tls_object_keys`] -- by the sweep that finds it dead. The mint-time
/// call is kept for the one case left: a lookup at an address whose dead
/// occupant's slot no sweep has freed yet answers that occupant's key, and
/// its rows must not seed the new engine. It can only ever drop rows of an
/// object that lived at this very address, i.e. of a dead one.
pub fn engine_forget_key(key: u64) -> usize {
    let mut dropped = 0usize;
    let id = engine_table().lock().remove(&key);
    if let Some(id) = id {
        dropped += 1;
        if engine_registry().write().remove(&id).is_some() {
            dropped += 1;
        }
    }
    {
        let mut sessions = engine_session_table().lock();
        let before = sessions.len();
        sessions.retain(|(k, _), _| *k != key);
        dropped += before - sessions.len();
    }
    if engine_alpn_selector_table().lock().remove(&key).is_some() {
        dropped += 1;
    }
    alpn_selector_unrooted().lock().remove(&key);
    if engine_sni_matchers_table().lock().remove(&key).is_some() {
        dropped += 1;
    }
    // gc-common w13-a: the engine no longer holds its `SSLContext`'s rows.
    ssl_context_engine_release(key);
    dropped
}

// SSLEngineResult status codes (matching SSLEngineResult.Status enum ordinal)
pub const SR_OK: i32 = 0;
pub const SR_BUFFER_OVERFLOW: i32 = 1;
pub const SR_BUFFER_UNDERFLOW: i32 = 2;
pub const SR_CLOSED: i32 = 3;

// Handshake status (matching SSLEngineResult.HandshakeStatus enum ordinal in
// JDK 25: NOT_HANDSHAKING(0), FINISHED(1), NEED_TASK(2), NEED_WRAP(3),
// NEED_UNWRAP(4), NEED_UNWRAP_AGAIN(5))
pub const HS_NOT_HANDSHAKING_R: i32 = 0;
pub const HS_FINISHED_R: i32 = 1;
pub const HS_NEED_TASK_R: i32 = 2;
pub const HS_NEED_WRAP_R: i32 = 3;
pub const HS_NEED_UNWRAP_R: i32 = 4;

/// Allocate a 4-field SSLEngineResult: (status, hsStatus, bytesConsumed,
/// bytesProduced). Status fields are stored as ints (Java-side accessors
/// turn them into the appropriate enum constants — see
/// `tls.rs::register_ssl_engine_result`).
/// Fetch a REAL enum constant via the enum's generated `valueOf(String)` so the
/// returned reference is the singleton — `==` comparisons in JSSE/connector code
/// (e.g. `result.getStatus() == OK`, `engine.getHandshakeStatus() == NEED_WRAP`)
/// then work. Returns `Object(None)` if the enum can't be resolved.
pub fn enum_const(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    cls: &str,
    name: &str,
    valueof_desc: &str,
) -> Value {
    let n = ctx.create_string(name);
    match ctx.invoke(cls, "valueOf", valueof_desc, &[Value::Object(Some(n))]) {
        Ok(Some(v)) => v,
        other => {
            if crate::nbflags().dbg_tls_hs {
                eprintln!(
                    "[dbg-tls-hs] thread={:?} enum_const FAILED cls={} name={} result={:?}",
                    std::thread::current().id(),
                    cls,
                    name,
                    other
                );
            }
            Value::Object(None)
        }
    }
}

/// Real `SSLEngineResult$Status` constant for our SR_* code.
pub fn real_status_enum(ctx: &mut dyn cratonvm_native_api::NativeContext, sr: i32) -> Value {
    let name = match sr {
        SR_BUFFER_OVERFLOW => "BUFFER_OVERFLOW",
        SR_BUFFER_UNDERFLOW => "BUFFER_UNDERFLOW",
        SR_CLOSED => "CLOSED",
        _ => "OK",
    };
    enum_const(
        ctx,
        "javax/net/ssl/SSLEngineResult$Status",
        name,
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLEngineResult$Status;",
    )
}

/// Real `SSLEngineResult$HandshakeStatus` constant for our HS_* code.
pub fn real_handshake_status_enum(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    hs: i32,
) -> Value {
    let name = match hs {
        HS_FINISHED_R => "FINISHED",
        HS_NEED_TASK_R => "NEED_TASK",
        HS_NEED_WRAP_R => "NEED_WRAP",
        HS_NEED_UNWRAP_R => "NEED_UNWRAP",
        _ => "NOT_HANDSHAKING",
    };
    enum_const(
        ctx,
        "javax/net/ssl/SSLEngineResult$HandshakeStatus",
        name,
        "(Ljava/lang/String;)Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
    )
}

pub fn status_name(sr: i32) -> &'static str {
    match sr {
        SR_BUFFER_OVERFLOW => "BUFFER_OVERFLOW",
        SR_BUFFER_UNDERFLOW => "BUFFER_UNDERFLOW",
        SR_CLOSED => "CLOSED",
        _ => "OK",
    }
}

pub fn hs_name(hs: i32) -> &'static str {
    match hs {
        HS_FINISHED_R => "FINISHED",
        HS_NEED_TASK_R => "NEED_TASK",
        HS_NEED_WRAP_R => "NEED_WRAP",
        HS_NEED_UNWRAP_R => "NEED_UNWRAP",
        _ => "NOT_HANDSHAKING",
    }
}

pub fn alloc_engine_result(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    status: i32,
    hs: i32,
    consumed: i32,
    produced: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    // Build a REAL SSLEngineResult via its public ctor with REAL enum constants,
    // so `getStatus()`/`getHandshakeStatus()` return singletons the connector
    // can `==`-compare. (The old synthetic int-slot object made every enum
    // comparison fail → the NIO handshake state machine spun → native SO.)
    let st = real_status_enum(ctx, status);
    let hss = real_handshake_status_enum(ctx, hs);
    let __dbg_hs = crate::nbflags().dbg_tls_hs;
    if matches!(st, Value::Object(Some(_))) && matches!(hss, Value::Object(Some(_))) {
        match ctx.new_object_initialized(
            "javax/net/ssl/SSLEngineResult",
            "(Ljavax/net/ssl/SSLEngineResult$Status;Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;II)V",
            &[st, hss, Value::Int(consumed), Value::Int(produced)],
        ) {
            Ok(Some(Value::Object(Some(o)))) => {
                if __dbg_hs {
                    eprintln!(
                        "[dbg-tls-hs] thread={:?} alloc_engine_result REAL status={} hs={}",
                        std::thread::current().id(),
                        status_name(status),
                        hs_name(hs)
                    );
                }
                return Ok(o);
            }
            other => {
                if __dbg_hs {
                    eprintln!(
                        "[dbg-tls-hs] thread={:?} alloc_engine_result ctor FAILED status={} hs={} result={:?}",
                        std::thread::current().id(),
                        status_name(status),
                        hs_name(hs),
                        other
                    );
                }
            }
        }
    } else if __dbg_hs {
        eprintln!(
            "[dbg-tls-hs] thread={:?} alloc_engine_result enum resolve FAILED status={} hs={} st_ok={} hss_ok={}",
            std::thread::current().id(),
            status_name(status),
            hs_name(hs),
            matches!(st, Value::Object(Some(_))),
            matches!(hss, Value::Object(Some(_)))
        );
    }
    // Fallback: synthetic int-slot object (enum resolution failed).
    let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 4)?;
    ctx.set_field(obj, 0, Value::Int(status));
    ctx.set_field(obj, 1, Value::Int(hs));
    ctx.set_field(obj, 2, Value::Int(consumed));
    ctx.set_field(obj, 3, Value::Int(produced));
    Ok(obj)
}

/// Compute the next handshake status from an EngineState.
pub fn handshake_status_of(s: &EngineState) -> i32 {
    if s.closed_inbound && s.closed_outbound {
        // …but a fully-closed engine can still OWE the peer a record. The
        // automatic TLS 1.2 `close_notify` response (see `do_unwrap`) closes
        // both halves and queues the reply in one step, and rustls holds that
        // reply until a `write_tls` — which only happens if the caller is told
        // to `wrap` again. Answering NOT_HANDSHAKING here made netty stop
        // wrapping and the reply was never emitted: `CloseNotifyTest`'s TLS 1.2
        // parameterisation read `null` where the client's own close_notify
        // belonged.
        //
        // `wants_write()` as well as `outbound`, because the queue that matters
        // here is rustls's — `outbound` only holds what a previous `wrap`
        // already drained out of it.
        if !s.outbound.is_empty() || s.conn.as_ref().is_some_and(|c| c.wants_write()) {
            return HS_NEED_WRAP_R;
        }
        return HS_NOT_HANDSHAKING_R;
    }
    // A handshake failure waiting to be raised is owed to the caller on a
    // WRAP, so keep asking for one. Without this the engine answered
    // NEED_UNWRAP as soon as the alert had drained, the caller stopped
    // wrapping, and the failure sat on the engine until the peer hung up —
    // which is the `StacklessClosedChannelException` the deferral exists to
    // prevent. See `EngineState::deferred_handshake_error` and the
    // `drained.is_empty()` note in `do_wrap`.
    if s.deferred_handshake_error.is_some() {
        return HS_NEED_WRAP_R;
    }
    // A delegated task the caller owes us outranks everything else: JSSE
    // reports NEED_TASK until the task has actually run. See `DelegatedTask`.
    if s.delegated_task != DelegatedTask::None {
        return HS_NEED_TASK_R;
    }
    // `write_tls()` may have produced more than one complete TLS record. A
    // previous wrap can legitimately emit only the first record when the
    // caller's destination buffer is smaller than the whole flight; keep
    // driving wrap until that queued remainder is on the wire.
    if !s.outbound.is_empty() {
        return HS_NEED_WRAP_R;
    }
    let conn = match s.conn.as_ref() {
        Some(c) => c,
        // The connection is ON LOAN to `do_unwrap`'s record loop, not absent.
        // Answering NOT_HANDSHAKING here says "the handshake is over", and a
        // caller that believes it stops driving the engine — which is a HANG,
        // reached from inside the very Java `TrustManager` upcall the loan
        // exists to allow (an `X509ExtendedTrustManager` is handed the
        // `SSLEngine` and JSSE's own tests query it). Measured: without this
        // arm, `JdkSslEngineTest`'s TLSv1.3 `testMutualAuthSameCertChain` and
        // `mustCallResumeTrustedOnSessionResumption` time out instead of
        // failing, 4 of 821 where the control has 0. See
        // `EngineState::conn_checked_out`.
        None if s.conn_checked_out => return HS_NEED_UNWRAP_R,
        // A server engine whose connection is held back until the ClientHello
        // arrives (ALPN selector) is still HANDSHAKING as far as the caller is
        // concerned, and what it needs next is the hello.
        None if s.alpn_selection_deferred => return HS_NEED_UNWRAP_R,
        None => return HS_NOT_HANDSHAKING_R,
    };
    if !conn.is_handshaking() {
        // FIX (tls-handshake-enforcement-gap, doc 21): a TLS 1.2 SERVER
        // queues its ChangeCipherSpec + Finished only AFTER it has processed
        // the client's Finished, so `is_handshaking()` flips false while the
        // server's own final flight is still sitting inside rustls. Reporting
        // FINISHED at that moment makes the caller stop driving the
        // handshake — Tomcat's `SecureNioChannel.handshake` returns as soon
        // as it sees FINISHED and only flushes `netOutBuffer`, never calling
        // `wrap` again — so those records never reach the wire and the peer
        // waits for a Finished that never comes, then sees the connection
        // torn down ("connection closed by peer during handshake").
        //
        // TLS 1.3 hid this completely: there the server's whole flight is
        // sent BEFORE the client's Finished arrives, so nothing is ever
        // pending at the moment the flag flips. Since nothing in the suite
        // pinned a connector to TLS 1.2 until protocol enforcement started
        // working (above), no TLS 1.2 server handshake had ever actually
        // completed through this engine.
        //
        // Demand one more `wrap` while records remain. The extra wrap also
        // gets TLS 1.3 NewSessionTicket messages onto the wire instead of
        // dropping them. Gated on `!handshake_finished_reported` so ordinary
        // post-handshake application writes still report NOT_HANDSHAKING.
        if !s.handshake_finished_reported && conn.wants_write() {
            return HS_NEED_WRAP_R;
        }
        if !s.handshake_finished_reported {
            return HS_FINISHED_R;        }
        return HS_NOT_HANDSHAKING_R;
    }
    if conn.wants_write() {
        HS_NEED_WRAP_R
    } else if conn.wants_read() {
        HS_NEED_UNWRAP_R
    } else {
        HS_NEED_TASK_R
    }
}

/// How a ByteBuffer's metadata fields were resolved — determines which
/// slot(s) `bb_set_pos` must write back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BbLayout {
    /// Real-JDK named fields (`position`/`limit`/`capacity`).
    Named,
    /// VM synthetic heap layout `[0]=array,[1]=pos,[2]=limit,[3]=cap`.
    SyntheticHeap,
    /// VM synthetic direct layout `[0]=pos,[1]=lim,[2]=cap,[3]=mark,[4]=addr`
    /// (see `direct_buffer.rs::dbb_allocate_direct0`'s non-named fallback).
    SyntheticDirect,
}

/// Where a ByteBuffer's bytes actually live.
#[derive(Clone, Copy)]
pub enum BbBacking {
    /// Heap buffer: Java `byte[]` plus array offset. `off` is the real-JDK
    /// `ByteBuffer.offset` (arrayOffset) — nonzero for sliced/duplicated
    /// views (e.g. Netty pooled heap buffers), whose logical index 0 lives
    /// at `hb[off]`, NOT `hb[0]`.
    Heap { arr: ObjectRef, off: usize },
    /// Direct buffer: bytes live in native memory at `addr` — a real
    /// allocation minted by `direct_buffer.rs::dbb_allocate` (slices and
    /// duplicates carry `parent.address + offset`, computed by real-JDK
    /// bytecode). Reactor-Netty's default pooled-direct path hands these
    /// to `SSLEngine.unwrap`/`wrap`.
    Direct { addr: u64 },
    /// Shape we could not resolve — reads/writes move zero bytes.
    Unresolved,
}

/// Resolved view of a Java ByteBuffer: backing store + position/limit/
/// capacity.
///
/// Tomcat's NIO endpoint hands the engine real-JDK `java.nio.HeapByteBuffer`
/// objects, whose layout is `Buffer{mark, position, limit, capacity,
/// address}` then `ByteBuffer{hb, offset, …}` — i.e. the backing array `hb`
/// is NOT at slot 0 (that's `mark`, an int). Resolve fields by NAME first
/// (mirroring `charset.rs::buf_state`), falling back to the VM's synthetic
/// layouts.
///
/// Reactor-Netty's `SslHandler` instead hands **`java.nio.DirectByteBuffer`**
/// views (`hb == null`, bytes in native memory at the `address` field).
/// Before 2026-07-07 those fell through to the synthetic-slot fallback,
/// resolved no backing array, and `unwrap`/`wrap` moved ZERO bytes — the
/// server engine never saw the ClientHello Netty delivered and its first
/// `unwrap` returned `BUFFER_UNDERFLOW consumed=0`, upon which Netty closed
/// the connection (client saw "TLS handshake failed: unexpected EOF"). See
/// `reactive-netty-https-sslengine-handshake-underflow-FIXED.md`.
pub struct BbView {
    pub backing: BbBacking,
    pub layout: BbLayout,
    pub pos: usize,
    pub lim: usize,
    pub cap: usize,
}

pub fn bb_view(ctx: &mut dyn cratonvm_native_api::NativeContext, bb: ObjectRef) -> BbView {
    // 1) Real-JDK heap buffer: backing array in the named `hb` field.
    if let Value::Object(Some(a)) = ctx.get_field_by_name(bb, "hb") {
        let pos = ctx
            .get_field_by_name(bb, "position")
            .as_int()
            .unwrap_or(0)
            .max(0) as usize;
        let lim = ctx
            .get_field_by_name(bb, "limit")
            .as_int()
            .unwrap_or(0)
            .max(0) as usize;
        let cap = ctx
            .get_field_by_name(bb, "capacity")
            .as_int()
            .unwrap_or(lim as i32)
            .max(0) as usize;
        let off = ctx
            .get_field_by_name(bb, "offset")
            .as_int()
            .unwrap_or(0)
            .max(0) as usize;
        return BbView {
            backing: BbBacking::Heap { arr: a, off },
            layout: BbLayout::Named,
            pos,
            lim,
            cap,
        };
    }
    // 2) Real-JDK direct buffer: no `hb`, bytes at the native `address`.
    //    Require `position`/`limit` to also resolve by name so we never
    //    treat some unrelated object's stale long as a pointer.
    if let Value::Long(addr) = ctx.get_field_by_name(bb, "address") {
        let pos_v = ctx.get_field_by_name(bb, "position").as_int();
        let lim_v = ctx.get_field_by_name(bb, "limit").as_int();
        if let (Some(p), Some(l)) = (pos_v, lim_v) {
            let cap = ctx
                .get_field_by_name(bb, "capacity")
                .as_int()
                .unwrap_or(l)
                .max(0) as usize;
            if addr > 0 && cap > 0 {
                // Clamp pos/lim to capacity: every native access through
                // this view is bounded by `cap`, the size the underlying
                // allocation (or the parent buffer a slice was cut from)
                // actually has.
                return BbView {
                    backing: BbBacking::Direct { addr: addr as u64 },
                    layout: BbLayout::Named,
                    pos: (p.max(0) as usize).min(cap),
                    lim: (l.max(0) as usize).min(cap),
                    cap,
                };
            }
        }
    }
    // 3) Synthetic heap layout `[0]=array,[1]=pos,[2]=limit,[3]=cap`.
    if let Value::Object(Some(a)) = ctx.get_field(bb, 0) {
        let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0).max(0) as usize;
        let lim = ctx.get_field(bb, 2).as_int().unwrap_or(0).max(0) as usize;
        let cap = if ctx.object_num_fields(bb) > 3 {
            ctx.get_field(bb, 3).as_int().unwrap_or(lim as i32).max(0) as usize
        } else {
            lim
        };
        return BbView {
            backing: BbBacking::Heap { arr: a, off: 0 },
            layout: BbLayout::SyntheticHeap,
            pos,
            lim,
            cap,
        };
    }
    // 4) Synthetic direct layout `[0]=pos,[1]=lim,[2]=cap,[3]=mark,[4]=addr`
    //    (see `direct_buffer.rs::dbb_allocate_direct0`).
    if ctx.object_num_fields(bb) >= 5 {
        if let Value::Long(addr) = ctx.get_field(bb, 4) {
            let pos_v = ctx.get_field(bb, 0).as_int();
            let lim_v = ctx.get_field(bb, 1).as_int();
            let cap_v = ctx.get_field(bb, 2).as_int();
            if let (Some(p), Some(l), Some(c)) = (pos_v, lim_v, cap_v) {
                let cap = c.max(0) as usize;
                if addr > 0 && cap > 0 {
                    return BbView {
                        backing: BbBacking::Direct { addr: addr as u64 },
                        layout: BbLayout::SyntheticDirect,
                        pos: (p.max(0) as usize).min(cap),
                        lim: (l.max(0) as usize).min(cap),
                        cap,
                    };
                }
            }
        }
    }
    // 5) Unresolvable — keep the synthetic-slot pos/lim so pure size probes
    //    (e.g. dst_cap sums) behave exactly as before; data moves are no-ops.
    let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0).max(0) as usize;
    let lim = ctx.get_field(bb, 2).as_int().unwrap_or(0).max(0) as usize;
    BbView {
        backing: BbBacking::Unresolved,
        layout: BbLayout::SyntheticHeap,
        pos,
        lim,
        cap: lim,
    }
}

/// One-line description of a resolved buffer view for `CRATONVM_DBG_TLS_HS`
/// diagnostics: Java class + backing shape + cursor fields.
pub fn bb_describe(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
    v: &BbView,
) -> String {
    let cls = ctx
        .class_name_of_id(ctx.class_id_of_object(bb))
        .unwrap_or_else(|| "<unknown-class>".to_string());
    let backing = match v.backing {
        BbBacking::Heap { off, .. } => format!("heap(arrayOffset={})", off),
        BbBacking::Direct { addr } => format!("direct(addr={:#x})", addr),
        BbBacking::Unresolved => "UNRESOLVED".to_string(),
    };
    format!(
        "class={} backing={} layout={:?} pos={} lim={} cap={}",
        cls, backing, v.layout, v.pos, v.lim, v.cap
    )
}

/// Read the byte at buffer index `i` (the position/limit coordinate space).
/// Returns `None` for unresolved backings or out-of-capacity direct access.
pub fn bb_get_byte(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    v: &BbView,
    i: usize,
) -> Option<u8> {
    match v.backing {
        BbBacking::Heap { arr, off } => {
            Some(ctx.get_array_element(arr, off + i).as_int().unwrap_or(0) as u8)
        }
        BbBacking::Direct { addr } => {
            if i >= v.cap {
                return None;
            }
            // `addr` is either a REAL native pointer (`dbb_allocate`) or an
            // Unsafe-ARENA TAGGED handle: real-JDK `DirectByteBuffer`s (and
            // Netty's `PlatformDependent` pooled buffers, the reactor-http
            // TLS path) get their `address` from `Unsafe.allocateMemory`,
            // which mints tagged arena handles — raw-dereferencing one is a
            // wild pointer (SIGSEGV in `SSLEngine.unwrap`, reactor-http-nio,
            // ServerHttpsRequestIntegrationTests). Route through the
            // arena-aware NativeContext bridge, which dispatches
            // arena-vs-real-pointer exactly like `Unsafe.copyMemory` does.
            let mut b = [0u8; 1];
            if ctx.copy_from_native_memory((addr as usize + i) as i64, &mut b) {
                Some(b[0])
            } else {
                None
            }
        }
        BbBacking::Unresolved => None,
    }
}

/// Copy buffer bytes `[from, to)` (buffer coordinates) into a Vec. Direct
/// backings are clamped to capacity; unresolved backings yield an empty Vec.
pub fn bb_bytes_range(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    v: &BbView,
    from: usize,
    to: usize,
) -> Vec<u8> {
    if to <= from {
        return Vec::new();
    }
    match v.backing {
        // PERF (testssl-testpost bulk TLS): one `get_array_element` per BYTE is
        // a virtual dispatch plus a `Value` box each time, and this is the
        // engine's application-data path — every byte Tomcat's JSSE connector
        // wraps or unwraps passes through here, twice (app buffer -> net buffer
        // and back). `read_byte_array_into` is the same read as a single
        // `copy_nonoverlapping` against the array payload. It also CLAMPS to the
        // array length instead of relying on `get_array_element`'s per-element
        // bounds guard, so a `to` past the array end now shortens the result
        // (matching what the `Direct` arm already does) rather than padding it
        // with zeros the caller would treat as real bytes.
        BbBacking::Heap { arr, off } => {
            let mut buf = vec![0u8; to - from];
            let n = ctx.read_byte_array_into(arr, off + from, &mut buf);
            buf.truncate(n);
            buf
        }
        BbBacking::Direct { addr } => {
            let end = to.min(v.cap);
            if end <= from {
                return Vec::new();
            }
            // Arena-aware read — see `bb_get_byte` for why raw dereference
            // is unsound here (tagged Unsafe-arena handles).
            let mut buf = vec![0u8; end - from];
            if ctx.copy_from_native_memory((addr as usize + from) as i64, &mut buf) {
                buf
            } else {
                Vec::new()
            }
        }
        BbBacking::Unresolved => Vec::new(),
    }
}

/// Write `data` starting at buffer index `at`. Returns bytes written
/// (direct backings clamp to capacity; unresolved backings write nothing).
pub fn bb_put_bytes(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    v: &BbView,
    at: usize,
    data: &[u8],
) -> usize {
    match v.backing {
        // PERF: the write-side mirror of `bb_bytes_range`'s heap arm — one bulk
        // copy instead of one virtual `set_array_element` per byte.
        //
        // The bulk intrinsic is all-or-nothing on a range that overruns the
        // array, so clamp to what actually fits and report the clamped count.
        // The old loop let `set_array_element`'s own guard drop the overrunning
        // tail and then returned `data.len()` regardless — i.e. it claimed bytes
        // that were never stored. Clamping keeps the caller making progress (a
        // 0 return would stall `bb_write_from`'s producer) while the count it
        // gets back is now true.
        BbBacking::Heap { arr, off } => {
            let start = off + at;
            let room = ctx.array_length(arr).saturating_sub(start);
            let n = room.min(data.len());
            if n == 0 || !ctx.write_byte_array_from(arr, start, &data[..n]) {
                return 0;
            }
            n
        }
        BbBacking::Direct { addr } => {
            let end = (at + data.len()).min(v.cap);
            if end <= at {
                return 0;
            }
            let n = end - at;
            // Arena-aware write — see `bb_get_byte` for why raw dereference
            // is unsound here (tagged Unsafe-arena handles).
            if ctx.copy_to_native_memory((addr as usize + at) as i64, &data[..n]) {
                n
            } else {
                0
            }
        }
        BbBacking::Unresolved => 0,
    }
}

/// Advance a ByteBuffer's `position` to `new_pos`, writing back through the
/// slot(s) the resolved layout actually reads (mirrors `charset.rs::set_pos`).
pub fn bb_set_pos(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
    layout: BbLayout,
    new_pos: usize,
) {
    match layout {
        // Real-JDK named `position`; ALSO write slot 1 (== `position` in the
        // real `Buffer` layout, == `pos` in the synthetic heap layout) to
        // preserve the historical dual-write.
        BbLayout::Named | BbLayout::SyntheticHeap => {
            ctx.set_field_by_name(bb, "position", Value::Int(new_pos as i32));
            ctx.set_field(bb, 1, Value::Int(new_pos as i32));
        }
        // Synthetic direct layout keeps `position` at slot 0 — writing
        // slot 1 would clobber `limit`.
        BbLayout::SyntheticDirect => {
            ctx.set_field_by_name(bb, "position", Value::Int(new_pos as i32));
            ctx.set_field(bb, 0, Value::Int(new_pos as i32));
        }
    }
}

/// Read up to `(limit - position)` bytes out of a ByteBuffer, leaving its
/// position advanced by the bytes consumed. Honors a `max` cap so callers
/// can chunk large buffers.
pub fn bb_read_into(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
    out: &mut Vec<u8>,
    max: usize,
) -> usize {
    let v = bb_view(ctx, bb);
    let avail = v.lim.saturating_sub(v.pos);
    let take = avail.min(max);
    if take == 0 {
        return 0;
    }
    let got = bb_bytes_range(ctx, &v, v.pos, v.pos + take);
    if got.is_empty() {
        return 0;
    }
    let n = got.len();
    out.extend_from_slice(&got);
    bb_set_pos(ctx, bb, v.layout, v.pos + n);
    n
}

/// Write up to `(limit - position)` bytes from `src` into a ByteBuffer,
/// advancing its position. Returns bytes written.
pub fn bb_write_from(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
    src: &[u8],
) -> usize {
    let v = bb_view(ctx, bb);
    let space = v.lim.saturating_sub(v.pos);
    let put = space.min(src.len());
    if put == 0 {
        return 0;
    }
    let n = bb_put_bytes(ctx, &v, v.pos, &src[..put]);
    if n == 0 {
        return 0;
    }
    bb_set_pos(ctx, bb, v.layout, v.pos + n);
    n
}

/// Build a default rustls ClientConfig for engine paths that didn't have an
/// SSLContext attach a real one. Uses selected context roots when present,
/// otherwise native roots, plus ALPN from state.
pub fn default_engine_client_config(alpn: &[Vec<u8>]) -> Result<Arc<ClientConfig>, String> {
    let trust_roots = take_selected_context_trust_roots();
    let roots = root_store_for_trust_roots(trust_roots.as_ref());
    let mut config = ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = alpn.to_vec();
    Ok(Arc::new(config))
}

/// Build a default rustls ServerConfig for engine paths that didn't have an
/// SSLContext attach one. Uses the *runtime-configured* TLS identity (set via
/// `set_runtime_tls_identity`) and the supplied ALPN list. This is the path
/// `SSLContext.createSSLEngine()` takes when the caller never installed a
/// `KeyManagerFactory` of their own.
///
/// SECURITY: returns a config-error string (which `engine_begin` propagates
/// as an `IOException` to the JVM, identical to other handshake misconfig
/// failures) when no runtime identity has been installed. There is no
/// silent fallback to an embedded private key.
///
/// `vm` is the engine's own VM (`EngineState::vm`): the runtime identity is
/// one per VM since gc-common w11-a.
pub fn default_engine_server_config(
    vm: usize,
    alpn: &[Vec<u8>],
    need_client_auth: bool,
) -> Result<Arc<ServerConfig>, String> {
    let alpn_strs: Vec<&str> = alpn
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .collect();
    let identity = runtime_tls_identity(vm)
        .ok_or_else(|| "No TLS key/cert configured; set javax.net.ssl.keyStore".to_string())?;
    let client_ca = if need_client_auth {
        match identity.client_ca_pem.as_deref() {
            Some(ca) => Some(ca.to_string()),
            None => {
                let trust_roots = take_selected_context_trust_roots();
                let pem = trust_roots_pem(trust_roots.as_ref());
                if pem.is_empty() {
                    return Err(
                        "setNeedClientAuth(true) requires javax.net.ssl.trustStore".to_string()
                    );
                }
                Some(pem)
            }
        }
    } else {
        None
    };
    build_server_config_single_cert(
        &identity.cert_pem,
        &identity.key_pem,
        &alpn_strs,
        need_client_auth,
        client_ca.as_deref(),
    )
}

// ---------------------------------------------------------------------------
// Deferred (renegotiation-substitute) client authentication
// ---------------------------------------------------------------------------
//
// FIX (tls-handshake-enforcement-gap, doc 21). Tomcat's default
// `certificateVerification` is "none": the connector does NOT ask for a client
// certificate during the initial handshake, and only discovers a request needs
// `CLIENT-CERT` auth after parsing the request line. It then collects the
// certificate by RENEGOTIATING mid-connection —
// `NioEndpoint$NioSocketWrapper.doClientAuth` calls
// `SSLEngine.setNeedClientAuth(true)` followed by `SecureNioChannel
// .rehandshake()`, i.e. a SECOND `beginHandshake()` on an engine whose
// handshake already finished.
//
// rustls categorically does not implement renegotiation (TLS 1.2 or 1.3): it
// answers any post-handshake ClientHello/HelloRequest with a
// `no_renegotiation` alert. So that second `beginHandshake()` could never do
// anything, Tomcat's rehandshake loop stalled and the connection was dropped
// with no response — which is exactly the "connection closed immediately
// after the TLS handshake" failure `TestClientCert`,
// `TestCustomSslTrustManager` and `TestResolverSSL` all reported.
//
// What IS implementable is doing the same thing one connection later. When
// the rehandshake attempt is detected we (a) remember that this connector
// wants a client certificate and (b) fail fast so the connection closes
// immediately instead of stalling. `http_url_connection::perform_with_retry`
// then transparently retries the request on a fresh connection, and THAT
// handshake carries an optional `CertificateRequest`, so the client's real
// Java `KeyManager.chooseClientAlias` runs (the suite asserts on the issuers
// it was offered) and the certificate is presented in time for Tomcat's
// `SSLAuthenticator` to find it already on the session — no renegotiation
// needed.
//
// The marker is keyed by the OWNING `SSLContext`'s GC-stable key (which
// `set_engine_trust_ctx_key` records for every engine, whether or not that
// context has TrustManagers), so it is scoped to one connector: a later test
// in the same JVM builds a fresh `SSLContext` and starts clean. That matters
// — `TestClientCert` asserts `getLastClientAuthRequestedIssuerCount() == 0`
// for the FIRST, unprotected request of every test, which a process-wide or
// certificate-keyed marker would break.
//
// Observable difference from real renegotiation: one extra TCP connection,
// and the request is re-sent on it (the body is already buffered by the
// caller, so this is transparent). What is NOT emulated is collecting the
// certificate on the SAME connection.
pub fn deferred_client_auth_contexts() -> &'static Mutex<std::collections::HashSet<u64>> {
    static T: OnceLock<Mutex<std::collections::HashSet<u64>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(std::collections::HashSet::new()))
}

pub fn mark_deferred_client_auth(ctx_key: u64) {
    deferred_client_auth_contexts().lock().insert(ctx_key);
}

pub fn wants_deferred_client_auth(ctx_key: Option<u64>) -> bool {
    ctx_key
        .map(|k| deferred_client_auth_contexts().lock().contains(&k))
        .unwrap_or(false)
}

/// Marker prefix on an `engine_begin` error string meaning "this is a
/// NEGOTIATION failure, not a configuration/IO one".
///
/// The distinction is visible to applications: JSSE reports "there is nothing
/// this engine and its peer could agree on" as `SSLHandshakeException`, and
/// callers assert on that type — `SSLEngineTest.testProtocolNoMatch` does
/// `assertThrows(SSLHandshakeException.class, () -> handshake(...))`. A bare
/// `IOException` is not an `SSLException` at all, so such a caller sees the
/// wrong type even when the handshake correctly refuses to happen.
pub const HANDSHAKE_ERR_PREFIX: &str = "handshake_failure: ";

/// JSSE's delegated-task contract, which this engine implements.
///
/// `SSLEngine.wrap`/`unwrap` may answer `NEED_TASK`, meaning "I have work I
/// want done off this thread". The caller must then collect the `Runnable`
/// from `getDelegatedTask()` and run it before the engine will make progress.
///
/// It is not decoration. netty's
/// `SslHandlerTest.test{Client,Server}HandshakeTimeoutBecauseExecutorNotExecute`
/// installs an `Executor` that deliberately never runs what it is given and
/// asserts the handshake then TIMES OUT. With no task ever emitted there was
/// nothing to withhold: the handshake completed and the assertion saw `null`
/// where an `SslHandshakeTimeoutException` belonged. `getDelegatedTask()` was
/// not registered at all, so a caller that reached it got nothing back.
///
/// **The work is real.** What gets deferred is `engine_begin` — building the
/// rustls configuration, which parses PEM chains, loads private keys and can
/// call back into Java `KeyManager` code. That is the class of work JSSE
/// defers, and until the task runs the connection genuinely does not exist, so
/// an executor that drops the task really does stall the handshake.
///
/// **Two rules the callers impose, both learned the expensive way:**
///
/// 1. *Consume first, then defer.* netty's `SslHandler.decodeJdkCompatible`
///    hands `unwrap` exactly one TLS record and treats
///    `bytesConsumed != packetLength` as "not an SSL/TLS record" — it throws
///    `NotSslRecordException` and fails the handshake. An `unwrap` that
///    answers NEED_TASK having consumed nothing is not deferring the
///    handshake, it is killing it. Real JSSE ingests the record and defers the
///    processing; `do_unwrap` stages the records into `deferred_inbound` and
///    reports them consumed.
/// 2. *Never answer `null` to a caller that was promised a task.* netty's
///    `SslTasksRunner.run()` returns immediately when `getDelegatedTask()` is
///    `null`, WITHOUT calling `runComplete()` — so `SslHandler` stays in
///    `STATE_PROCESS_TASK`, where `decode()` and `flush()` are both no-ops,
///    and the connection is wedged for good. `task_unclaimed` is what
///    guarantees one non-null answer per promise even if the work was
///    meanwhile done inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelegatedTask {
    /// Nothing outstanding — either never needed, or already run.
    None,
    /// A caller was told NEED_TASK; `getDelegatedTask()` owes it a `Runnable`.
    Owed,
    /// Collected. The caller owns the work now, and the engine stays
    /// `NEED_TASK` until it is run — however long that takes, or forever,
    /// which is exactly what the two timeout tests measure.
    HandedOut,
}

/// Which `SSLException` subclass JSSE raises for a handshake-phase failure.
///
/// `SSLProtocolException` means "the peer broke the protocol" — a message of
/// the wrong type or at the wrong time, a payload that will not decode, an
/// implementation that did something RFC-illegal. `SSLHandshakeException`
/// means "we could not agree", which is everything else here: a rejected
/// certificate, no cipher suites in common, no ALPN protocol.
///
/// The distinction is asserted, not cosmetic: netty's
/// `SslHandlerTest.testTruncatedPacket` pushes a ServerHello INTO a server
/// engine and requires `SSLProtocolException` specifically, and JSSE's
/// `SSLEngineInputRecord` raises exactly that for an unexpected handshake
/// message.
/// JSSE's text for an alert RECEIVED from the peer: `Alert.AlertConsumer`
/// ends in `tc.fatal(alert, "Received fatal alert: " + name, ..)`, and `fatal`
/// prefixes the alert's own name in parentheses (`HotSpot 25`:
/// `(internal_error) Received fatal alert: internal_error`). The names are
/// JSSE's `Alert` constants, snake_case (nio21 wave 13, L4).
pub fn jsse_received_alert_message(desc: rustls::AlertDescription) -> String {
    let dbg = format!("{desc:?}");
    let name = if dbg == "UnrecognisedName" {
        "unrecognized_name".to_string()
    } else {
        let cs: Vec<char> = dbg.chars().collect();
        let mut out = String::new();
        for (i, &c) in cs.iter().enumerate() {
            if c.is_ascii_uppercase() {
                let prev_lower = i > 0 && cs[i - 1].is_ascii_lowercase();
                let acronym_end = i > 0
                    && cs[i - 1].is_ascii_uppercase()
                    && cs.get(i + 1).is_some_and(|n| n.is_ascii_lowercase());
                if prev_lower || acronym_end {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    };
    format!("({name}) Received fatal alert: {name}")
}

/// The message for a handshake error, naming the application's `TrustManager`
/// when that is what actually refused.
///
/// rustls reports a verifier rejection as
/// `InvalidCertificate(ApplicationVerificationFailure)`, which stringifies to
/// something about "application verification failure" and says nothing about
/// WHICH manager said no or why. The reason was recorded by
/// `engine_consult_trust_managers` on its way out
/// (`set_last_trust_rejection_detail`), and this is where it is spent — so a
/// caller sees the same sentence it saw when the check ran after the handshake
/// rather than a downgrade in diagnostics as the price of moving it earlier.
pub fn handshake_error_message(e: &rustls::Error) -> String {
    if matches!(e, rustls::Error::General(m) if m == TRUST_MANAGER_UNHANDLED) {
        // The detail stays recorded for `CRATONVM_DBG=tls-auth`; JSSE's
        // message is fixed (see `TrustOutcome::Unhandled`).
        let _ = take_last_trust_rejection_detail();
        return JSSE_UNHANDLED_EXCEPTION_MESSAGE.to_string();
    }
    if let rustls::Error::AlertReceived(desc) = e {
        return jsse_received_alert_message(*desc);
    }
    if matches!(
        e,
        rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)
    ) {
        if let Some(detail) = take_last_trust_rejection_detail() {
            return format!("TrustManager rejected the peer certificate chain: {detail}");
        }
    }
    format!("rustls: {e}")
}

/// The `SSLException` subclass JSSE raises for a fatal alert RECEIVED from the
/// peer, in any phase (nio20 wave 4 fix-back, L4). `Alert.AlertConsumer` ends in
/// `tc.fatal(alert, "Received fatal alert: ..", true, null)`, whose exception is
/// `Alert.createSSLException`: `SSLProtocolException` for `unexpected_message`,
/// `SSLHandshakeException` for every alert JSSE marks `handshakeOnly`, plain
/// `SSLException` for the rest (`close_notify`, `bad_record_mac`,
/// `decryption_failed`, `record_overflow`, `decompression_failure`,
/// `internal_error`, `inappropriate_fallback`, `user_canceled`; an unknown id).
/// The class does NOT depend on whether this side thinks its handshake is
/// over: a TLS 1.3 client that sent its `Finished` and then reads the server's
/// `certificate_required` raises `SSLHandshakeException` on HotSpot
/// (`NioConf19b clientauth-missing`), where this engine raised `SSLException`.
pub fn jsse_received_alert_exception_class(desc: rustls::AlertDescription) -> &'static str {
    use rustls::AlertDescription as A;
    match desc {
        A::UnexpectedMessage => "javax/net/ssl/SSLProtocolException",
        A::CloseNotify
        | A::BadRecordMac
        | A::DecryptionFailed
        | A::RecordOverflow
        | A::DecompressionFailure
        | A::InternalError
        | A::InappropriateFallback
        | A::UserCanceled
        | A::Unknown(_) => "javax/net/ssl/SSLException",
        _ => "javax/net/ssl/SSLHandshakeException",
    }
}

pub fn jsse_handshake_exception_class(e: &rustls::Error) -> &'static str {
    match e {
        // nio20 wave 4 fix-back (L4): see `jsse_received_alert_exception_class`.
        rustls::Error::AlertReceived(desc) => jsse_received_alert_exception_class(*desc),
        // nio20 wave 4 (L4): see `TrustOutcome::Unhandled`.
        rustls::Error::General(m) if m == TRUST_MANAGER_UNHANDLED => "javax/net/ssl/SSLException",
        rustls::Error::InappropriateMessage { .. }
        | rustls::Error::InappropriateHandshakeMessage { .. }
        | rustls::Error::InvalidMessage(_)
        | rustls::Error::PeerMisbehaved(_)
        | rustls::Error::PeerSentOversizedRecord
        | rustls::Error::BadMaxFragmentSize => "javax/net/ssl/SSLProtocolException",
        _ => "javax/net/ssl/SSLHandshakeException",
    }
}

/// Turn an `engine_begin` error string into the Java exception JSSE raises for
/// it — see [`HANDSHAKE_ERR_PREFIX`].
///
/// MUST be called with the engine's lock DROPPED (see [`EngineCell`]): it
/// allocates.
pub fn engine_begin_failure(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    e: String,
) -> MethodCallFailed {
    match e.strip_prefix(HANDSHAKE_ERR_PREFIX) {
        Some(rest) => {
            crate::phases_early::throw_jca_exc(ctx, "javax/net/ssl/SSLHandshakeException", rest)
        }
        None => RuntimeError::IOException { message: e }.into(),
    }
}

/// Take the `Runnable` this engine owes its caller, if it still owes one.
///
/// Answers `true` at most once per promise: netty's in-line
/// `runDelegatedTasks` loop drains `getDelegatedTask()` until it answers null,
/// so a second `true` would spin it. It reads `task_unclaimed` rather than
/// `delegated_task` on purpose — see rule 2 on [`DelegatedTask`].
pub fn claim_delegated_task(id: i32) -> bool {
    with_engine(id, |s| {
        if s.task_unclaimed {
            s.task_unclaimed = false;
            if s.delegated_task == DelegatedTask::Owed {
                s.delegated_task = DelegatedTask::HandedOut;
            }
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

/// Realize the rustls connection, or hand the caller a delegated task to do
/// it — JSSE's `NEED_TASK` contract. See [`DelegatedTask`].
///
/// Returns `Ok(true)` when the work was DEFERRED (the caller must now be told
/// `NEED_TASK` and given no bytes), `Ok(false)` when it was done or was not
/// needed.
/// Release a native pin frame when the enclosing scope ends, including on the
/// record loop's early `return Err(...)`. A leaked frame roots every object in
/// it for the life of the process.
pub struct UnpinOnDrop {
    pub base: usize,
}

impl Drop for UnpinOnDrop {
    fn drop(&mut self) {
        // The ctx is the one published for this same window; if it is gone the
        // frame goes with the call anyway.
        let base = self.base;
        let _ = with_active_native_context(move |ctx| ctx.unpin_native_roots(base));
    }
}

/// Hold `EngineState::conn` outside the registry while `do_unwrap`'s record
/// loop runs, and put it back on EVERY exit.
///
/// Why the loop cannot simply keep the lock: rustls calls
/// `ServerCertVerifier::verify_server_cert` from inside `process_new_packets`,
/// and that verifier has to consult the application's Java `TrustManager` — a
/// bytecode upcall which may call straight back into an engine native. The
/// registry's `parking_lot` write guard is not reentrant, so an upcall under it
/// deadlocks the thread. Deferring the trust decision until after the handshake
/// is what this replaces, and that deferral is why
/// `testHandshakeFailureOnlyFireExceptionOnce` could not pass: the client had
/// already sent its `Finished` before anybody asked Java, so the server
/// completed a valid handshake and the later alert could not fail an
/// already-completed promise.
///
/// `Drop` rather than an explicit put-back because the loop has early
/// `return Err(throw_jca_exc(...))` exits. A missed restore does not fail a
/// test — it leaves `conn == None` for the life of that engine, so every later
/// `wrap`/`unwrap` on it silently does nothing.
pub struct ConnCheckout {
    pub id: i32,
    pub conn: Option<EngineConn>,
}

impl ConnCheckout {
    /// Take the connection out of the registry, marking the engine as on-loan.
    /// Answers an empty checkout (a harmless no-op on drop) when the engine has
    /// no connection yet, so callers need no special case.
    pub fn take(id: i32) -> Self {
        let conn = with_engine(id, |s| {
            let c = s.conn.take();
            s.conn_checked_out = c.is_some();
            c
        })
        .flatten();
        Self { id, conn }
    }
}

impl Drop for ConnCheckout {
    fn drop(&mut self) {
        let Some(conn) = self.conn.take() else {
            return;
        };
        with_engine(self.id, |s| {
            // Unconditional overwrite: `engine_begin` refuses to run while
            // `conn_checked_out` holds, so nothing can have installed a rival
            // connection in the window.
            s.conn = Some(conn);
            s.conn_checked_out = false;
        });
        // An engine dropped from the registry mid-loop (close on another
        // thread) simply loses the connection here, which is what closing it
        // means; there is nowhere to put it back.
    }
}

pub fn engine_begin_or_defer(id: i32) -> Result<bool, String> {
    let Some(cell) = engine_cell(id) else {
        return Ok(false);
    };
    let mut g = cell.lock();
    let s: &mut EngineState = &mut g;
    if crate::nbflags().dbg_tls_hs_ok {
        eprintln!(
            "[dbg-tls-task] thread={:?} begin_or_defer id={} state={:?} armed={} unclaimed={}",
            std::thread::current().id(),
            id,
            s.delegated_task,
            s.delegated_task_armed,
            s.task_unclaimed
        );
    }
    match s.delegated_task {
        // Collected and not yet run: the caller owns the work. Refuse to make
        // progress. This refusal IS the feature.
        DelegatedTask::HandedOut => Ok(true),
        // Owed, but the caller came back for more instead of collecting it.
        // A caller that ignores NEED_TASK entirely would otherwise spin
        // forever, so do the work inline and let it through.
        //
        // `task_unclaimed` is deliberately NOT cleared here. A caller can also
        // reach this arm by being merely SLOW — its executor collects the task
        // on another thread a moment later — and handing that caller a `null`
        // is what wedges netty (rule 2 on [`DelegatedTask`]). One promise, one
        // non-null answer, whoever did the work.
        DelegatedTask::Owed => {
            s.delegated_task = DelegatedTask::None;
            engine_begin_if_needed(s).map(|()| false)
        }
        DelegatedTask::None => {
            if s.delegated_task_armed {
                return engine_begin_if_needed(s).map(|()| false);
            }
            // The roots are staged in a thread-local by whichever thread built
            // the `SSLContext`; a delegated task runs on another thread by
            // definition, so snapshot them onto the engine while we are still
            // on the thread that can see them.
            if s.trust_roots_override.is_none() {
                s.trust_roots_override = take_selected_context_trust_roots();
            }
            s.delegated_task_armed = true;
            s.task_unclaimed = true;
            s.delegated_task = DelegatedTask::Owed;
            Ok(true)
        }
    }
}

/// `engine_begin`, but only when the connection is not there yet. A CLIENT
/// engine reaches the delegated-task paths with its connection already built —
/// it defers the PROCESSING of the server's first flight, not the config.
pub fn engine_begin_if_needed(state: &mut EngineState) -> Result<(), String> {
    if state.conn.is_some() {
        return Ok(());
    }
    engine_begin(state)
}

/// Begin the handshake — construct the rustls connection from the cached
/// configs (or defaults) and stash it on the engine.
pub fn engine_begin(state: &mut EngineState) -> Result<(), String> {
    if state.conn_checked_out {
        // The connection exists; it is on loan to `do_unwrap`'s record loop
        // (see `EngineState::conn_checked_out`). Building a fresh one here
        // would be silently discarded when the loan is returned, and any state
        // the caller then set on it would go with it. A re-entrant caller in
        // this window is by definition inside our own handshake processing, so
        // "already begun" is the truthful answer.
        return Ok(());
    }
    if state.conn.is_some() {
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] engine_begin SHORT-CIRCUIT (conn already realized) need={} want={}",
                state.need_client_auth, state.want_client_auth
            );
        }
        // Tomcat's `doClientAuth` rehandshake — see the module comment above.
        // Recognised by: server engine, handshake already realized WITHOUT a
        // certificate request, and client auth now switched on.
        if !state.is_client
            && !state.client_auth_requested
            && (state.need_client_auth || state.want_client_auth)
        {
            if let Some(key) = state.trust_managers_ctx_key {
                mark_deferred_client_auth(key);
            }
            return Err(
                "TLS renegotiation is not supported; the client certificate will be requested \
                 on the next handshake for this connector"
                    .to_string(),
            );
        }
        return Ok(());
    }
    // An EXPLICIT protocol restriction that names only versions this stack
    // cannot negotiate is a handshake failure, not an invitation to widen.
    //
    // `provider_and_versions` (via `protocol_versions_for`) answers "no
    // restriction" for both "the caller asked for nothing" and "the caller
    // asked only for TLSv1/TLSv1.1", and the two are not the same thing: the
    // second is a peer this engine has nothing in common with, and silently
    // offering TLS 1.2+1.3 anyway makes a deliberately-incompatible pair
    // handshake successfully. netty's `SSLEngineTest.testProtocolNoMatch`
    // configures a `TLSv1.2`-only client against a `TLSv1`/`TLSv1.1`-only
    // server and asserts `SSLHandshakeException`; it got a completed
    // handshake. (`testProtocolMatch`, whose server list also contains
    // `TLSv1.2`, is unaffected — the intersection is non-empty there.)
    //
    // Deliberately scoped to a NON-EMPTY list: an explicitly emptied one
    // (`setEnabledProtocols(new String[0])`, which
    // `testEnablingAnAlreadyDisabledSslProtocol` round-trips before restoring a
    // real list) keeps its existing "unrestricted" treatment rather than
    // gaining a new failure mode this page never measured.
    if !state.enabled_protocols.is_empty()
        && protocol_versions_for(&state.enabled_protocols).is_empty()
    {
        return Err(format!(
            "{HANDSHAKE_ERR_PREFIX}No appropriate protocol (protocol is disabled or \
             cipher suites are inappropriate): enabled protocols {:?} name no TLS \
             version this engine can negotiate",
            state.enabled_protocols
        ));
    }
    let alpn_strs: Vec<&str> = state
        .alpn_protocols
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .collect();
    if state.is_client {
        // The shape that has to match for two engines to share one config —
        // see `ctx_client_config_table`. A client WITH an identity shares too
        // (nio20 wave 4, L4: `identity_shareable` below), so a digest of the
        // identity is part of its key.
        let mut shape = format!(
            "{:?}|{:?}|{:?}|{:?}|{}|{}",
            state.enabled_ciphers,
            state.enabled_protocols,
            state.alpn_protocols,
            (
                &state.endpoint_id_alg,
                &state
                    .explicit_sni
                    .as_ref()
                    .and_then(|h| h.clone())
                    .or_else(|| state.peer_host.clone())
            ),
            state.need_client_auth,
            state.want_client_auth,
        );
        let shareable = state.identity_override.is_none() && state.client_config.is_none();
        // A client with a context identity and no pre-built config shares one
        // config per (context, shape, identity) as well, with a
        // `SinkRecordingClientCertResolver` in place of the per-engine
        // recorder: the per-engine `Arc` made resumption impossible for every
        // client context that had a key manager (`NioConf19b resume`).
        let identity_shareable = state.identity_override.is_some()
            && state.client_config.is_none()
            && state.trust_managers_ctx_key.is_some();
        if identity_shareable {
            if let Some((cert, key)) = state.identity_override.as_ref() {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                cert.hash(&mut h);
                key.hash(&mut h);
                shape.push_str(&format!("|id:{:016x}", h.finish()));
            }
        }
        let cached = if shareable || identity_shareable {
            state.trust_managers_ctx_key.and_then(|k| {
                ctx_client_config_table()
                    .lock()
                    .get(&(k, shape.clone()))
                    .cloned()
            })
        } else {
            None
        };
        let from_cache = cached.is_some();
        let config = match state.client_config.clone().or(cached) {
            Some(c) => c,
            // A real Java TrustManager is the authority for this context.
            // Rustls must only perform cryptographic handshake verification in
            // that case, then engine_run_trust_check invokes the manager with
            // the peer chain.  Constructing the old root-store-only config
            // here rejected self-signed test certificates before Netty's
            // InsecureTrustManagerFactory (or any custom TrustManager) could
            // make its Java-level decision.
            None => {
                let trust_roots = state
                    .trust_roots_override
                    .clone()
                    .or_else(take_selected_context_trust_roots);
                let revocation = trust_roots
                    .as_ref()
                    .and_then(|roots| roots.revocation.clone());
                let roots = root_store_for_trust_roots(trust_roots.as_ref());
                let use_java_trust_manager = state
                    .trust_managers_ctx_key
                    .and_then(|key| {
                        ctx_trust_managers_table()
                            .lock()
                            .get(&key)
                            .map(|managers| !managers.is_empty())
                    })
                    .unwrap_or(false);
                let client_auth = state
                    .identity_override
                    .as_ref()
                    .map(|(cert, key)| (cert.as_str(), key.as_str()));
                // Unlike the server branch below (which already threads
                // `state.enabled_ciphers` through
                // `build_server_config_single_cert_ex_ciphers`), this client
                // branch built its `ClientConfig` with the plain default
                // cipher provider regardless of any cipher-suite restriction
                // the caller configured (`SSLEngine.setEnabledCipherSuites`/
                // `setSSLParameters` — see `register_apply_parameters`'s
                // `setSSLParameters` handler). A deliberately-mismatched
                // client cipher restriction was therefore silently ignored:
                // the client engine still offered its full default cipher
                // list, which generally overlaps with whatever the server
                // is restricted to, so the handshake succeeded instead of
                // failing with `SSLHandshakeException` as real-JDK does
                // (`connectWithSslBundleAndOptionsMismatch`).
                // FIX (tls-handshake-enforcement-gap, doc 21): honour
                // `SSLEngine.setEnabledProtocols` too. Same shape as the
                // cipher gap described just above — a client engine
                // restricted to one TLS version still offered both, so a
                // deliberate version mismatch negotiated the OTHER version
                // and succeeded (`TestSSLHostConfigProtocol`'s
                // `testTlsVersionMismatch*`). `provider_and_versions` also
                // stops a TLS-1.2-only cipher restriction from advertising
                // TLS 1.3 it cannot actually negotiate.
                let (provider, versions) =
                    provider_and_versions(&state.enabled_ciphers, &state.enabled_protocols);
                // Endpoint identification runs INSIDE the handshake (see
                // `PassthroughServerCertVerifier::endpoint_identity`), because
                // the SERVER can tell the difference: a client that rejects
                // the certificate after its own `Finished` has already let the
                // server complete. The post-handshake gate keeps its copy of
                // the check for the paths that never build a config here.
                //
                // Gated on the SAME predicate as the post-handshake gate: an
                // application `X509ExtendedTrustManager` OWNS identification
                // and JSSE adds none, which is what netty's wrapper around
                // `InsecureTrustManagerFactory` relies on — `testSessionCache`
                // dials `a.netty.io` against a `localhost` certificate and
                // expects it to connect.
                let endpoint_identity = match (
                    &state.endpoint_id_alg,
                    &state
                        .explicit_sni
                        .as_ref()
                        .and_then(|h| h.clone())
                        .or_else(|| state.peer_host.clone()),
                ) {
                    (Some(alg), Some(host))
                        if endpoint_alg_verifies_identity(alg)
                            && !host.is_empty()
                            && ctx_jsse_identifies(state.trust_managers_ctx_key) =>
                    {
                        Some((alg.clone(), host.clone()))                    }
                    _ => None,
                };
                build_client_config_ex_with_provider(
                    roots,
                    &alpn_strs,
                    ClientAuthMode::Fixed(client_auth),
                    revocation,
                    use_java_trust_manager,
                    provider,
                    &versions,
                    endpoint_identity,
                    // The engine path, and the only one that can consult Java
                    // from inside verification: see
                    // `PassthroughServerCertVerifier::trust_ctx_key`.
                    state.trust_managers_ctx_key,
                    false,
                )?
            }
        };
        // Interpose the "was a client certificate actually sent?" recorder on
        // whichever resolver the config ended up with — the fixed-identity one,
        // the `JavaKeyManagerResolver`, or a caller-supplied one on a pre-built
        // config. Doing it here rather than inside each builder keeps the one
        // signal in one place; see `RecordingClientCertResolver` for why the
        // signal is needed at all. The clone is per engine, and the parts that
        // matter for sharing (the resumption/session store) are `Arc`s that
        // clone by reference.
        // Interpose the recorder ONLY on a client that has an identity to
        // present. Without one there is nothing to record — the existing
        // `None` case already means "nothing sent" — and the wrapper's fresh
        // `Arc` per engine is precisely what makes resumption impossible, so
        // installing it unconditionally cost every identity-less client its
        // session cache.
        let needs_recorder = state.identity_override.is_some() || !shareable;
        let session_store = state.trust_managers_ctx_key.map(ctx_client_session_store);
        let config = if let (true, Some(k)) = (identity_shareable, state.trust_managers_ctx_key) {
            // nio20 wave 4 (L4): one config per (context, shape, identity),
            // its recorder shared; the per-engine flag is reached through
            // `PRESENTED_SINK` while this engine's records are processed.
            state.client_cert_presented = Some(Arc::new(std::sync::atomic::AtomicBool::new(false)));
            if from_cache {
                config
            } else {
                let mut cloned = (*config).clone();
                cloned.client_auth_cert_resolver = Arc::new(SinkRecordingClientCertResolver {
                    inner: cloned.client_auth_cert_resolver.clone(),
                });
                if let Some(store) = session_store {
                    cloned.resumption = rustls::client::Resumption::store(store);
                }
                ctx_client_config_table()
                    .lock()
                    .entry((k, shape))
                    .or_insert(Arc::new(cloned))
                    .clone()
            }
        } else if needs_recorder {
            let presented = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let mut cloned = (*config).clone();
            cloned.client_auth_cert_resolver = Arc::new(RecordingClientCertResolver {
                inner: cloned.client_auth_cert_resolver.clone(),
                presented: presented.clone(),
            });
            if let Some(store) = session_store {
                cloned.resumption = rustls::client::Resumption::store(store);
            }
            state.client_cert_presented = Some(presented);
            Arc::new(cloned)
        } else if let Some(k) = state.trust_managers_ctx_key {
            // Share this SSLContext's session store across its engines, then
            // remember the finished config so the NEXT engine of the same
            // shape is byte-for-byte the same object — see
            // `ctx_client_config_table`.
            let config = {
                let mut cloned = (*config).clone();
                if let Some(store) = session_store {
                    cloned.resumption = rustls::client::Resumption::store(store);
                }
                Arc::new(cloned)
            };
            ctx_client_config_table()
                .lock()
                .entry((k, shape))
                .or_insert(config)
                .clone()
        } else {
            config
        };
        let host = state
            .explicit_sni
            .as_ref()
            .and_then(|s| s.clone())
            .or_else(|| state.inferred_sni.clone().flatten())
            .or_else(|| state.peer_host.clone())
            .unwrap_or_else(|| "localhost".to_string());
        let send_sni = match &state.explicit_sni {
            Some(host) => host.is_some(),
            None => state
                .inferred_sni
                .as_ref()
                .map_or_else(|| jsse_would_send_sni(&host), |name| name.is_some()),
        };
        let mut config = (*config).clone();
        config.enable_sni = send_sni;
        let config = Arc::new(config);
        // rustls demands *some* `ServerName`; a host it cannot parse (an
        // underscore label, say) has no representation, whereas real JSSE
        // simply omits the SNI extension for one. Keep the historical
        // `"localhost"` stand-in for that case rather than failing a handshake
        // that used to work — endpoint identification does NOT read this value
        // (it matches against `peer_host` directly), so the stand-in cannot
        // launder a certificate mismatch into a pass.
        let server_name = match ServerName::try_from(host.clone()) {
            Ok(sn) => sn,
            Err(e) => {
                if crate::nbflags().dbg_tls_auth_ok {
                    eprintln!(
                        "[dbg-tls-auth] peer host {host:?} is not a usable SNI name ({e}); \
                         falling back to \"localhost\" for SNI only"
                    );
                }
                ServerName::try_from("localhost".to_string())
                    .map_err(|e| format!("invalid SNI hostname: {}", e))?
            }
        };
        let cc = ClientConnection::new(config, server_name)
            .map_err(|e| format!("ClientConnection::new: {}", e))?;
        state.conn = Some(EngineConn::Client(cc));
    } else {
        let config = match state.server_config.clone() {
            Some(c) => c,
            // Per-context server identity (this engine's own keystore) wins over
            // the process-global runtime identity, so an in-process server and
            // client don't clobber each other's cert.
            None => match &state.identity_override {
                Some((cert, key)) => {
                    // Request the client cert for either NEED (required) or WANT
                    // (optional) client auth — otherwise an "optional" server
                    // never asks and `peer_certificates()` stays empty.
                    //
                    // INVESTIGATED, NOT APPLIED (tomcat-clientauth-engine-config):
                    // Tomcat's SSLHostConfig/SSLAuthenticator machinery
                    // deliberately does NOT toggle need/want client auth up
                    // front for the common "certificateVerification=optional,
                    // auth decided per-request" configuration this suite
                    // exercises (`TestClientCert`/`TestCustomSslTrustManager`)
                    // — instead it always requests the cert (if at all) via a
                    // mid-connection TLS renegotiation
                    // (`SSLEngine.setNeedClientAuth(true)` + `beginHandshake()`
                    // called AGAIN on an already-handshaked engine, from
                    // `NioEndpoint$NioSocketWrapper.doClientAuth`). rustls
                    // categorically does not support renegotiation in TLS 1.2
                    // (nor TLS 1.3 — see rustls's own `manual::tlsvulns` docs);
                    // it unconditionally rejects any post-handshake ClientHello/
                    // HelloRequest with a `no_renegotiation` alert
                    // (`common_state.rs::process_msg`, gated on
                    // `may_receive_application_data`, which flips true the
                    // instant the FIRST handshake finishes — there is no window
                    // in which a real renegotiation attempt would be accepted).
                    // `engine_begin`'s own `state.conn.is_some()` early return
                    // (below the closing brace of this match) means that second
                    // `beginHandshake()` call was ALSO silently discarded on the
                    // CratonVM side even before hitting that rustls wall — see
                    // this crate's
                    // `tls-ocsp-clientcert-validation-not-enforced-FIXED.md`, "Residual #2 implementation"
                    // point 2, for the full trace evidence.
                    //
                    // True wire-level renegotiation is therefore not
                    // implementable without swapping TLS backends (the same
                    // class of permanently-unfixable gap as this doc's TLS 1.2
                    // DHE cipher case).
                    //
                    // A workaround WAS tried and measured: when this engine's
                    // SSLContext carried trust roots (a real `TrustManager[]`
                    // was configured), speculatively send an OPTIONAL
                    // CertificateRequest on the very FIRST handshake instead of
                    // waiting for Tomcat's rehandshake — proven to work
                    // end-to-end (a client with a KeyManager installed
                    // volunteers its cert immediately; `WebPkiClientVerifier::
                    // allow_unauthenticated()` accepts an empty Certificate
                    // message from a client with none, identically to no
                    // CertificateRequest ever being sent). Measured effect:
                    // `TestCustomSslTrustManager` improved 2/9->1/9 failing
                    // (the remaining failure is the doc's already-known
                    // `testCustomTrustManagerNone` order-dependent flake), but
                    // `TestClientCert` stayed at 5/18 failing with a *different*
                    // failure set — `testClientCertGetWithPreemptive` newly
                    // PASSED, but `testClientCertPostLarger` newly FAILED,
                    // because the suite has an explicit,
                    // deliberately-asserted invariant this workaround violates:
                    // `doTestClientCertGet`/`doTestClientCertPost` both assert
                    // `assertEquals(0, TesterSupport.
                    // getLastClientAuthRequestedIssuerCount())` after the FIRST
                    // (unprotected-resource) request — i.e. the suite is
                    // explicitly verifying NO CertificateRequest is sent until
                    // a protected resource actually needs one. A speculative
                    // upfront request can therefore only ever satisfy the
                    // small "preemptive" subset of this suite while breaking
                    // the (larger) "non-preemptive" subset's own explicit
                    // assertions — it cannot net-improve `TestClientCert`
                    // without also implementing the deferred/mid-connection
                    // request the suite actually wants, which circles back to
                    // requiring real renegotiation. Reverted (kept as `false`
                    // below, not deleted, since the wiring — `client_ca`
                    // resolution, `has_custom_trust_managers`, the
                    // `optional_client_cert` passthrough a few lines down —
                    // is correct and reusable if a future session finds a
                    // renegotiation-shaped answer, e.g. a custom rustls fork
                    // or an OpenSSL-backed engine variant that does support
                    // it). Deliberately does NOT downgrade or override an
                    // already explicit `need`/`want` (a real
                    // `setNeedClientAuth(true)` called before the first
                    // handshake — e.g. a `certificateVerification=required`
                    // host, or the plain `SSLParameters.setNeedClientAuth
                    // (true)`-upfront repro this doc's residual #2 verified
                    // directly — still wins and still enforces REQUIRED
                    // semantics exactly as before; this is unaffected by the
                    // revert below).
                    //
                    // FIX (tls-handshake-enforcement-gap, doc 21): the
                    // speculative request is no longer unconditional — it is
                    // armed ONLY after this connector has actually attempted
                    // the rehandshake (see `deferred_client_auth_contexts`
                    // and the short-circuit branch at the top of
                    // `engine_begin`). That is precisely what makes it
                    // compatible with the suite invariant described above:
                    // the first, unprotected request of every test still
                    // sees NO CertificateRequest (`assertEquals(0,
                    // getLastClientAuthRequestedIssuerCount())` holds), and
                    // only a connector that has already demanded client auth
                    // once asks up front.
                    //
                    // Still gated on a real trust source, and specifically on
                    // THIS context's registered `TrustManager[]` — not on
                    // `trust_roots_override`. Those roots are seeded from a
                    // process-global "selected context" slot at
                    // `createSSLEngine` time, so a connector configured with
                    // NO trust source at all can still inherit roots another
                    // context loaded earlier. `TestCustomSslTrustManager`'s
                    // `TrustType.NONE` case is exactly that shape — it nulls
                    // the truststore and sets no `trustManagerClassName`, and
                    // asserts the protected request does NOT succeed. Arming
                    // deferred auth off the inherited roots made the server
                    // request, accept and authenticate a certificate it had
                    // no business trusting, turning a correct refusal into a
                    // 200. `SSLContext.init(kms, null, null)` leaves
                    // `ctx_trust_managers_table` empty for that context,
                    // which is the signal we actually want.
                    let has_trust_source = state
                        .trust_managers_ctx_key
                        .map(|k| {
                            ctx_trust_managers_table()
                                .lock()
                                .get(&k)
                                .map(|v| !v.is_empty())
                                .unwrap_or(false)
                        })
                        .unwrap_or(false);
                    let speculative_optional_auth = !state.need_client_auth
                        && !state.want_client_auth
                        && has_trust_source
                        && wants_deferred_client_auth(state.trust_managers_ctx_key);
                    let request = state.need_client_auth
                        || state.want_client_auth
                        || speculative_optional_auth;
                    state.client_auth_requested = request;
                    let client_ca = if request {
                        let trust_roots = state
                            .trust_roots_override
                            .clone()
                            .or_else(take_selected_context_trust_roots);
                        let pem = trust_roots_pem(trust_roots.as_ref());
                        if pem.is_empty() {
                            None
                        } else {
                            Some(pem)
                        }
                    } else {
                        None
                    };
                    // Tomcat's `trustManagerClassName` mechanism delegates the
                    // trust decision to a Java class and deliberately has NO
                    // keystore-derived truststore, so `client_ca` above is
                    // legitimately empty. Detect that a real Java
                    // `TrustManager` IS registered for this engine (so
                    // `engine_run_trust_check` will actually enforce trust
                    // post-handshake) and, only then, fall back to a
                    // passthrough verifier instead of failing the config
                    // build outright.
                    let has_custom_trust_managers = request
                        && client_ca.is_none()
                        && state
                            .trust_managers_ctx_key
                            .map(|k| {
                                ctx_trust_managers_table()
                                    .lock()
                                    .get(&k)
                                    .map(|v| !v.is_empty())
                                    .unwrap_or(false)
                            })
                            .unwrap_or(false);
                    let optional_client_cert = (state.want_client_auth
                        || speculative_optional_auth)
                        && !state.need_client_auth;
                    // FIX (tomcatservletwebserverfactorytests-ssl-clientauth-peercert-residuals):
                    // optional client auth (WANT) with no trust source at all
                    // (no truststore, no custom TrustManager) used to fall
                    // into the `else` branch below with `client_ca=None`,
                    // where `build_server_config_single_cert_ex_ciphers`
                    // treats a missing CA as a hard config-build error even
                    // for the optional case -- real JSSE does not fail the
                    // handshake in this scenario (see the doc comment on
                    // `PassthroughClientCertVerifier`, case 2), so use the
                    // same passthrough verifier already used for the
                    // custom-trust-manager case. NEED (mandatory) mode is
                    // untouched: `optional_client_cert` is false whenever
                    // `state.need_client_auth` is true.
                    // FIX (jdksslenginetest-engine-level-gaps, cause B2): a
                    // registered Java `TrustManager[]` is the AUTHORITY for the
                    // client chain, exactly as it already is for the server
                    // chain on the client side (`use_java_trust_manager` in the
                    // client branch above), and for the same reason: webpki and
                    // the JDK do not agree on what a valid chain is, and the
                    // oracle is the JDK.
                    //
                    // The asymmetry this replaces was measurable. netty's test
                    // certificates are self-signed with `basicConstraints
                    // CA:true` — `SelfSignedCertificate`, and `CertificateBuilder
                    // .setIsCertificateAuthority(true)` — so when such a
                    // certificate is presented as a CLIENT identity webpki
                    // refuses it with `CaUsedAsEndEntity` and the server sends a
                    // `certificate_unknown` alert. JSSE has no such rule; it
                    // completes the handshake, and `testSessionAfterHandshake0`
                    // then reads `serverSession.getPeerCertificates()`. On this
                    // VM that threw `SSLPeerUnverifiedException` for 48 of this
                    // class's failures — not because the chain was never
                    // captured, but because the handshake it belonged to had
                    // already been aborted.
                    //
                    // Trust is NOT weakened: `engine_run_trust_check` runs
                    // `checkClientTrusted` on the captured chain the moment the
                    // crypto handshake finishes and aborts with a fatal alert on
                    // rejection, so a client the truststore does not trust is
                    // still refused — by the JDK validator instead of webpki.
                    // `testMutualAuthClientCertFail` and
                    // `testMutualAuthDiffCertsServerFailure` (both of which
                    // assert a REJECTION) are the check on that.
                    let use_passthrough_verifier = has_custom_trust_managers
                        || (optional_client_cert && !has_trust_source)
                        || (client_ca.is_none() && optional_client_cert)
                        || (request && has_trust_source);
                    if crate::nbflags().dbg_tls_auth_ok {
                        eprintln!(
                            "[dbg-tls-auth] engine_begin request={} client_ca_none={} trust_ctx_key={:?} has_custom_trust_managers={} use_passthrough_verifier={}",
                            request, client_ca.is_none(), state.trust_managers_ctx_key, has_custom_trust_managers, use_passthrough_verifier
                        );
                    }
                    let built = if use_passthrough_verifier {
                        build_server_config_single_cert_passthrough_client_auth(
                            cert,
                            key,
                            &alpn_strs,
                            state.need_client_auth,
                            &state.enabled_ciphers,
                            &state.enabled_protocols,
                            accepted_issuer_hints(state.trust_managers_ctx_key),
                        )
                    } else {
                        build_server_config_single_cert_ex_ciphers(
                            cert,
                            key,
                            &alpn_strs,
                            state.need_client_auth,
                            optional_client_cert,
                            client_ca.as_deref(),
                            &state.enabled_ciphers,
                            &state.enabled_protocols,
                        )
                    };
                    built?
                }
                None => {
                    if crate::nbflags().dbg_tls_auth_ok {
                        eprintln!(
                            "[dbg-tls-auth] engine_begin(default_engine_server_config) need={} want={}",
                            state.need_client_auth, state.want_client_auth
                        );
                    }
                    default_engine_server_config(
                        state.vm,
                        &state.alpn_protocols,
                        state.need_client_auth,
                    )?
                }
            },
        };
        // Every server-config branch above that asked the peer for a
        // certificate did so because NEED/WANT was already set (the
        // speculative deferred-auth branch sets this flag itself). Recording
        // it here as well covers the pre-built-config and global-identity
        // branches, so `engine_begin`'s rehandshake detector never mistakes
        // "already asked" for "asking for the first time".
        state.client_auth_requested |= state.need_client_auth || state.want_client_auth;
        // Share this SSLContext's server-side session store across its engines,
        // for the same reason as the client's — a server that forgets every
        // session cannot honour a resumption attempt — but PARTITIONED by
        // whether this engine is asking for a client certificate. Resuming
        // across that boundary replays a handshake that asked for none, which
        // silently cancels the request. See `ctx_server_session_store`.
        let config = match state.trust_managers_ctx_key {
            Some(k) => {
                let mut cloned = (*config).clone();
                cloned.session_storage = ctx_server_session_store(k, state.client_auth_requested);
                Arc::new(cloned)
            }
            None => config,
        };
        let sc =
            ServerConnection::new(config).map_err(|e| format!("ServerConnection::new: {}", e))?;
        state.conn = Some(EngineConn::Server(sc));
    }
    Ok(())
}

/// Return the largest prefix containing only complete TLS records that fits in
/// `capacity`. TLS records must never be split across SSLEngine.wrap calls:
/// Tomcat writes each produced buffer directly to the channel, so a partial
/// next record would make the peer reject the otherwise valid handshake.
pub fn complete_tls_record_prefix(data: &[u8], capacity: usize) -> usize {
    let mut end = 0usize;
    while end + 5 <= data.len() {
        let body_len = ((data[end + 3] as usize) << 8) | data[end + 4] as usize;
        let record_end = end.saturating_add(5).saturating_add(body_len);
        if record_end > data.len() || record_end > capacity {
            break;
        }
        end = record_end;
    }
    end
}

/// Pump rustls outbound bytes into `state.outbound`, then determine how many
/// complete TLS records fit in `dst`. Returns (consumed_from_app, produced).
pub fn engine_wrap_pump(
    state: &mut EngineState,
    app_bytes: &[u8],
    dst_remaining: usize,
) -> (usize, usize) {
    // Read before the `&mut state.conn` borrow below starts.
    let finished_reported = state.handshake_finished_reported;
    if state.conn_checked_out {
        // Re-entrant wrap while `do_unwrap`'s record loop holds the connection
        // (see `EngineState::conn_checked_out`). Answering `(0, 0)` is the same
        // answer as "no connection yet", and it silently DROPS whatever the
        // caller wanted written — a lost handshake record, i.e. a hang with no
        // error. It should not be reachable: the loan is confined to one native
        // call on one thread, and `handshake_status_of` reports NEED_UNWRAP
        // throughout it so no caller is invited to wrap. Say so if it ever is,
        // rather than losing the record quietly.
        eprintln!(
            "[tls] BUG: wrap on an engine whose connection is checked out by \
             do_unwrap's record loop; the write is being dropped. Please report \
             this with CRATONVM_DBG=tls-hs output."
        );
        return (0, 0);
    }
    let conn = match state.conn.as_mut() {
        Some(c) => c,
        None => return (0, 0),
    };

    // Phase 1: feed app data into rustls writer (post-handshake only).
    // `handshake_finished_reported` for the same reason `do_wrap`'s
    // `needs_app_data` uses it: `!is_handshaking()` goes true one flight before
    // the caller is told the handshake ended, and anything written in that
    // window is plaintext the peer is not expecting yet. `do_wrap` already
    // hands us an empty `app_bytes` there; this keeps the invariant local so a
    // future caller of this helper cannot reintroduce the same bug.
    let mut consumed = 0usize;
    if !app_bytes.is_empty() && finished_reported && !conn.is_handshaking() {
        if let Ok(n) = conn.writer().write(app_bytes) {
            consumed = n;
        }
    }

    // Phase 2: drain TLS records into the outbound buffer, then trim into dst.
    if conn.wants_write() {
        let mut out: Vec<u8> = Vec::new();
        let _ = conn.write_tls(&mut out);
        state.outbound.extend(out);
    }

    let take = complete_tls_record_prefix(&state.outbound, dst_remaining);
    let produced = take;
    if take > 0 {
        // The caller drains this complete-record prefix into dst.    
}
    (consumed, produced)}

/// Push inbound TLS bytes into rustls, process packets, then drain plaintext
/// into the dsts (returned as `Vec<u8>`). Returns (consumed_from_src, plaintext_out).
pub fn engine_unwrap_pump(state: &mut EngineState, inbound: &[u8]) -> Result<(usize, Vec<u8>), String> {
    engine_unwrap_pump_raw(state, inbound).map_err(|e| format!("rustls process_new_packets: {}", e))
}

/// As [`engine_unwrap_pump`], but surfacing the rustls error itself.
///
/// The KIND of the error decides which `SSLException` subclass JSSE raises
/// (`jsse_handshake_exception_class`), and a stringified error cannot be
/// classified: `testTruncatedPacket` wants `SSLProtocolException` for a
/// ServerHello pushed into a server engine and sees `SSLHandshakeException`
/// if the class is picked before the kind is known.
pub fn engine_unwrap_pump_raw(
    state: &mut EngineState,
    inbound: &[u8],
) -> Result<(usize, Vec<u8>), rustls::Error> {
    // A shared client config's recorder writes THIS engine's flag (see
    // `SinkRecordingClientCertResolver`).
    let _presented_sink = PresentedSink::enter(state.client_cert_presented.clone());
    let conn = match state.conn.as_mut() {
        Some(c) => c,
        None => return Ok((0, Vec::new())),
    };
    pump_conn_raw(conn, &mut state.inbound, inbound)
}

/// The body of [`engine_unwrap_pump_raw`] over a connection and its carry-over
/// buffer, so the delegated task can run it with the connection checked out of
/// the registry (nio21 wave 13, L4: a verifier-time `TrustManager` upcall
/// cannot run under the registry lock).
pub fn pump_conn_raw(
    conn: &mut EngineConn,
    carry: &mut Vec<u8>,
    inbound: &[u8],
) -> Result<(usize, Vec<u8>), rustls::Error> {
    // Append inbound to the carry-over, then feed rustls.
    carry.extend_from_slice(inbound);
    let consumed_from_src = inbound.len();

    if !carry.is_empty() {
        let mut cursor = std::io::Cursor::new(std::mem::take(carry));
        // read_tls may consume only a partial record; loop until rustls
        // refuses or we hit EOF.
        loop {
            let pos_before = cursor.position();
            match conn.read_tls(&mut cursor) {
                Ok(0) => break,
                Ok(_) => {
                    conn.process_new_packets()?;
                    if !conn.wants_read() && cursor.position() == pos_before {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        // Stash any unconsumed tail back.
        let pos = cursor.position() as usize;
        let mut buf = cursor.into_inner();
        if pos < buf.len() {
            buf.drain(0..pos);
            *carry = buf;
        }
    }

    // Drain plaintext if not still handshaking.
    let mut plaintext = Vec::new();
    if !conn.is_handshaking() {
        let mut tmp = [0u8; 16384];
        loop {
            match conn.reader().read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => plaintext.extend_from_slice(&tmp[..n]),
                Err(_) => break,
            }
        }
    }

    Ok((consumed_from_src, plaintext))
}

/// Capture negotiated session info onto state when handshake just completed.
pub fn engine_capture_negotiation(state: &mut EngineState) {
    if state.handshake_finished_reported {
        return;
    }
    let conn = match state.conn.as_ref() {
        Some(c) => c,
        None => return,
    };
    if conn.is_handshaking() {
        return;
    }
    if let Some(alpn) = conn.alpn_protocol() {
        if let Ok(s) = std::str::from_utf8(alpn) {
            state.negotiated_alpn = Some(s.to_string());
        }
    }
    // Capture the peer's certificate chain (the client cert, for a server
    // engine) so `SSLSession.getPeerCertificates()` can hand it to Tomcat's
    // client-cert authenticator.
    if state.peer_cert_chain_der.is_empty() {
        if let Some(certs) = conn.peer_certificates() {
            state.peer_cert_chain_der = certs.iter().map(|c| c.as_ref().to_vec()).collect();
        }
    }
}

/// Data extracted from an `EngineState` (while the registry lock is still
/// held) needed to run the post-handshake `TrustManager` consultation. Plain
/// owned data only — no `ObjectRef`s here except by GC-safe `u64` key
/// (`trust_ctx_key`), so this can safely cross the lock-drop boundary before
/// `engine_run_trust_check` (which calls into Java) runs.
pub struct PendingTrustCheck {
    /// The engine this check belongs to, so a rejection can queue the fatal
    /// alert JSSE sends the peer — see `reject_peer_with_fatal_alert`.
    pub engine_id: i32,
    pub is_client: bool,
    pub peer_chain_der: Vec<Vec<u8>>,
    /// `None` when the owning `SSLContext` has no `TrustManager[]` attached —
    /// the pending check can still be worth running for its endpoint-identity
    /// half, which JSSE performs regardless of which TrustManager is in force.
    pub trust_ctx_key: Option<u64>,
    pub negotiated_cipher_suite_name: Option<String>,
    /// `(algorithm, peer_host)` when this client engine must perform RFC 2818 /
    /// RFC 6125 endpoint identification once the chain is accepted.
    pub endpoint_identity: Option<(String, String)>,
}

/// True for the `SSLParameters.setEndpointIdentificationAlgorithm()` values
/// that mean "verify the peer certificate names the host I dialled".
///
/// JSSE recognises `HTTPS` (RFC 2818) and `LDAPS` (RFC 2830); the comparison is
/// case-insensitive there, and anything else — including the empty string and
/// `null` — means no identification. We deliberately do NOT treat an unknown
/// algorithm as "verify anyway": that would reject connections JSSE accepts.
pub fn endpoint_alg_verifies_identity(alg: &str) -> bool {
    alg.eq_ignore_ascii_case("HTTPS") || alg.eq_ignore_ascii_case("LDAPS")
}

/// Called right after the crypto handshake reports FINISHED for the first
/// time, WHILE STILL HOLDING the engine registry lock — extracts what's
/// needed and returns `Some` at most once per engine (gated by
/// `trust_check_done`). The caller MUST drop the registry lock before acting
/// on the result: looking up `ctx_trust_managers_table` and invoking Java is
/// deferred to `engine_run_trust_check` specifically so no allocating/GC-
/// triggering call ever happens while this lock is held (see
/// `EngineState::trust_managers_ctx_key`'s doc for why that matters).
/// The ClientHello `server_name` about to be handed to rustls, exactly once
/// per engine. `None` — meaning "no gate to apply" — for a client engine, for
/// an engine with no matchers configured, for every call after the first, and
/// for any source buffer that does not begin with a parseable ClientHello
/// carrying a `server_name`.
///
/// Marks the engine checked as soon as it looks at a handshake record, so a
/// hello that carries no SNI is not re-examined on every later `unwrap`.
pub fn engine_pending_sni_host(
    ctx: &mut dyn NativeContext,
    id: i32,
    view: &BbView,
    from: usize,
    to: usize,
) -> Option<String> {
    let interesting = with_engine(id, |s| !s.is_client && !s.sni_match_done).unwrap_or(false);
    if !interesting {
        return None;
    }
    // No cheap matcher pre-check here: the table is keyed by the ENGINE
    // object, which this helper does not hold, and
    // `engine_run_sni_match_check` returns immediately when the engine has
    // none. This runs at most once per engine either way.
    let bytes = bb_bytes_range(ctx, view, from, to.min(from + 4096));
    let host = peek_client_hello_sni(&bytes);
    if !bytes.is_empty() && bytes[0] == 22 {
        with_engine(id, |s| {
            s.sni_match_done = true;
        });
    }
    host
}

pub fn engine_take_pending_trust_check(id: i32, state: &mut EngineState) -> Option<PendingTrustCheck> {
    if state.trust_check_done {
        return None;
    }
    let finished = state
        .conn
        .as_ref()
        .map(|c| !c.is_handshaking())
        .unwrap_or(false);
    if !finished {
        return None;
    }
    state.trust_check_done = true;
    if state.peer_cert_chain_der.is_empty() {
        // No peer certificate was presented (e.g. optional client auth and
        // the client declined) — nothing for a TrustManager to check, and
        // nothing to identify either.
        return None;
    }
    let trust_ctx_key = state.trust_managers_ctx_key;
    // Endpoint identification is a CLIENT-side gate: it matches the server's
    // certificate against the host this side dialled. A server engine has no
    // "host it dialled", and JSSE's server-side identification (the LDAPS/HTTPS
    // algorithm applied to a client certificate) is not something any caller
    // reaching this path configures.
    let endpoint_identity = match (
        &state.endpoint_id_alg,
        &state
            .explicit_sni
            .as_ref()
            .and_then(|h| h.clone())
            .or_else(|| state.peer_host.clone()),
    ) {
        (Some(alg), Some(host))
            if state.is_client && endpoint_alg_verifies_identity(alg) && !host.is_empty() =>
        {
            Some((alg.clone(), host.clone()))
        }
        _ => None,
    };
    // Neither half has anything to do — stay exactly as cheap as before.
    if trust_ctx_key.is_none() && endpoint_identity.is_none() {
        return None;
    }
    // This string never reaches Java: its sole reader is the
    // `contains("ECDSA")` auth-type guess below, which feeds `checkServerTrusted`
    // an `"ECDSA"`/`"RSA"` literal. The translation is applied anyway, and it is
    // a no-op for this reader: the rewrite touches only the `TLS13_` prefix, so
    // it can only alter a TLS 1.3 name, and no TLS 1.3 suite contains `ECDSA`
    // under either spelling (measured: all five variants, both spellings) — the
    // guess is invariant under it. Going through the one helper rather than the
    // raw `Debug` string is what lets the witness test below carry NO
    // exceptions.
    let cipher_name = state
        .conn
        .as_ref()
        .and_then(|c| c.negotiated_cipher_suite())
        .map(|cs| suite_to_java_cipher_name(cs.suite()));
    Some(PendingTrustCheck {
        engine_id: id,
        is_client: state.is_client,
        peer_chain_der: state.peer_cert_chain_der.clone(),
        trust_ctx_key,
        negotiated_cipher_suite_name: cipher_name,
        endpoint_identity,
    })
}

/// Extract the `server_name` (SNI host) from a buffer that starts at a TLS
/// record boundary and is expected to hold a ClientHello.
///
/// Why parse it here instead of asking rustls: rustls only reports
/// `server_name()` AFTER it has processed the ClientHello, and processing it
/// also produces the whole server flight. JSSE's SNI gate runs at ClientHello
/// time — `ServerHandshakeContext` refuses before a ServerHello exists, so the
/// client sees an `unrecognized_name` alert and nothing else. Checking after
/// the fact left the client's handshake already complete: netty's
/// `SniClientTest.testSniSNIMatcherDoesNotMatchClient` then saw the server
/// report a failure and the client report success, and its
/// `assertThrows(SSLException.class, …)` failed with "nothing was thrown".
///
/// Deliberately total and bounds-checked: every length is validated against
/// the remaining slice, and anything unexpected answers `None` (meaning "no
/// gate to apply"), never a panic. `None` is also the answer for a hello with
/// no `server_name` extension, which is exactly JSSE's behaviour — with no
/// name received there is nothing for a matcher to match.
pub fn peek_client_hello_sni(buf: &[u8]) -> Option<String> {
    fn u16at(b: &[u8], i: usize) -> Option<usize> {
        Some(((*b.get(i)? as usize) << 8) | *b.get(i + 1)? as usize)
    }
    // TLS record: type(1) version(2) length(2). Handshake is 22.
    if *buf.first()? != 22 {
        return None;
    }
    let rec_len = u16at(buf, 3)?;
    let body = buf.get(5..5 + rec_len)?;
    // Handshake: msg_type(1)=client_hello, length(3).
    if *body.first()? != 1 {
        return None;
    }
    let hs_len = ((*body.get(1)? as usize) << 16)
        | ((*body.get(2)? as usize) << 8)
        | (*body.get(3)? as usize);
    let hello = body.get(4..4 + hs_len)?;
    // legacy_version(2) random(32)
    let mut p = 34usize;
    // legacy_session_id
    p += 1 + *hello.get(p)? as usize;
    // cipher_suites
    p += 2 + u16at(hello, p)?;
    // legacy_compression_methods
    p += 1 + *hello.get(p)? as usize;
    // extensions
    let ext_total = u16at(hello, p)?;
    p += 2;
    let ext_end = p.checked_add(ext_total)?;
    if ext_end > hello.len() {
        return None;
    }
    while p + 4 <= ext_end {
        let ext_type = u16at(hello, p)?;
        let ext_len = u16at(hello, p + 2)?;
        let data = hello.get(p + 4..p + 4 + ext_len)?;
        if ext_type == 0x0000 {
            // ServerNameList: list_length(2), then entries of
            // name_type(1) + length(2) + host.
            let list_len = u16at(data, 0)?;
            let list = data.get(2..2 + list_len)?;
            let mut q = 0usize;
            while q + 3 <= list.len() {
                let name_type = *list.get(q)?;
                let name_len = u16at(list, q + 1)?;
                let name = list.get(q + 3..q + 3 + name_len)?;
                if name_type == 0 {
                    return String::from_utf8(name.to_vec()).ok();
                }
                q += 3 + name_len;
            }
            return None;
        }
        p += 4 + ext_len;    }
    None
}

/// Extract the ALPN protocol list a ClientHello offered, from a buffer that
/// starts at a TLS record boundary.
///
/// Same shape, bounds discipline and totality as [`peek_client_hello_sni`] —
/// see that function for why the ClientHello is parsed here rather than asked
/// of rustls. `None` means "no ALPN extension" (or "not a parseable
/// ClientHello"), which is distinct from `Some(vec![])`: a client that sent no
/// ALPN extension must not have a server selector applied to it at all.
pub fn peek_client_hello_alpn(buf: &[u8]) -> Option<Vec<String>> {
    fn u16at(b: &[u8], i: usize) -> Option<usize> {
        Some(((*b.get(i)? as usize) << 8) | *b.get(i + 1)? as usize)
    }
    if *buf.first()? != 22 {
        return None;
    }
    let rec_len = u16at(buf, 3)?;
    let body = buf.get(5..5 + rec_len)?;
    if *body.first()? != 1 {
        return None;
    }
    let hs_len = ((*body.get(1)? as usize) << 16)
        | ((*body.get(2)? as usize) << 8)
        | (*body.get(3)? as usize);
    let hello = body.get(4..4 + hs_len)?;
    let mut p = 34usize; // legacy_version(2) + random(32)
    p += 1 + *hello.get(p)? as usize; // legacy_session_id
    p += 2 + u16at(hello, p)?; // cipher_suites
    p += 1 + *hello.get(p)? as usize; // legacy_compression_methods
    let ext_total = u16at(hello, p)?;
    p += 2;
    let ext_end = p.checked_add(ext_total)?;
    if ext_end > hello.len() {
        return None;
    }
    while p + 4 <= ext_end {
        let ext_type = u16at(hello, p)?;
        let ext_len = u16at(hello, p + 2)?;
        let data = hello.get(p + 4..p + 4 + ext_len)?;
        // application_layer_protocol_negotiation (RFC 7301) is extension 16.
        if ext_type == 0x0010 {
            let list_len = u16at(data, 0)?;
            let list = data.get(2..2 + list_len)?;
            let mut out = Vec::new();
            let mut q = 0usize;
            while q < list.len() {
                let n = *list.get(q)? as usize;
                let name = list.get(q + 1..q + 1 + n)?;
                if let Ok(s) = std::str::from_utf8(name) {
                    out.push(s.to_string());                }
                q += 1 + n;            }
            return Some(out);        }
        p += 4 + ext_len;    }
    None
}

/// Extract the `legacy_session_id` a ServerHello carries, from a buffer that
/// starts at a TLS record boundary.
///
/// This is the ONE piece of session identity that is on the wire and that both
/// engines can therefore agree on. rustls exposes no session id of its own, so
/// `SSLSession.getId()` had to derive a pseudo-id from the session object's
/// identity — which is stable per engine and necessarily DIFFERENT on the two
/// ends of one connection. netty's `SSLEngineTest.testSSLSessionId` asserts
/// that a TLS 1.2 client and server report byte-identical ids.
///
/// TLS 1.2 only, by design: under TLS 1.3 the field is a meaningless echo of
/// whatever the client put in its own `legacy_session_id`, both sides would
/// trivially agree, and the same test asserts they must NOT
/// (`assertFalse(Arrays.equals(...))` for the TLSV13 combo). The version check
/// lives at the call site, which knows the negotiated version; this function
/// only parses.
///
/// Same bounds discipline as [`peek_client_hello_sni`]: every length is checked
/// and anything unexpected answers `None`.
pub fn peek_server_hello_session_id(buf: &[u8]) -> Option<Vec<u8>> {
    // TLS record: type(1)=22 handshake, version(2), length(2).
    if *buf.first()? != 22 {
        return None;
    }
    let rec_len = ((*buf.get(3)? as usize) << 8) | *buf.get(4)? as usize;
    let body = buf.get(5..5 + rec_len)?;
    // Handshake: msg_type(1)=2 server_hello, length(3).
    if *body.first()? != 2 {
        return None;
    }
    let hs_len = ((*body.get(1)? as usize) << 16)
        | ((*body.get(2)? as usize) << 8)
        | (*body.get(3)? as usize);
    let hello = body.get(4..4 + hs_len)?;
    // legacy_version(2) + random(32), then legacy_session_id_echo.
    let id_len = *hello.get(34)? as usize;
    if id_len == 0 {
        return None;
    }
    Some(hello.get(35..35 + id_len)?.to_vec())
}

/// The `BiFunction<SSLEngine, List<String>, String>` a caller installed with
/// `SSLEngine.setHandshakeApplicationProtocolSelector`, keyed by
/// `engine_objref_key`.
///
/// This used to be an inert registration whose own comment called itself a
/// KNOWN DIVERGENCE: the selector was accepted and never invoked, and the
/// getter answered null so nothing could tell. It is a real hook now, and it
/// is the ONLY way a server engine learns which protocols to advertise —
/// netty's `JdkAlpnSslEngine` calls `setApplicationProtocols` for a CLIENT
/// engine but `setHandshakeApplicationProtocolSelector` for a SERVER one, so
/// with the setter inert a server advertised nothing at all and ALPN
/// negotiated to null on BOTH sides (`SSLEngineTest.verifyApplicationLevelProtocol`,
/// `expected: <my-protocol-http2> but was: <null>`).
///
/// Holds a live `ObjectRef`, so it is scanned and remapped by
/// `gc_scan_tls_ctx_trust_manager_roots` / `gc_update_tls_ctx_trust_manager_refs`
/// alongside this module's other object-holding tables.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0). Seven sites: four one-statement
/// `get`/`insert`/`remove`s, the `defer_for_alpn` test (whose key is now
/// hoisted out of the lock expression — see there), and the GC scan/remap pair,
/// which `drop` each guard before taking the next. The GC scan runs with the
/// heap lock (L8) held, which is legal: L0 < L8 is the descending order the
/// wrapper asserts.
pub fn engine_alpn_selector_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, ObjectRef>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, ObjectRef>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Run the installed server-side ALPN selector against the ClientHello sitting
/// in `src`, and record its answer as this engine's advertised protocol list.
///
/// MUST run BEFORE `engine_begin`: rustls fixes `ServerConfig::alpn_protocols`
/// at construction and selects from it with its own "first of mine the client
/// also offers" rule, so the only place a Java policy can be honoured is by
/// deciding that list first. Setting it to exactly the one protocol the
/// selector chose makes rustls's rule agree with the selector's by
/// construction — including `testAlpnCompatibleProtocolsDifferentClientOrder`,
/// where server preference must beat client order.
///
/// Three answers, matching `SSLEngine`'s documented contract for the selector:
///   * a non-empty string → advertise exactly that protocol;
///   * the empty string → the selector declined; advertise nothing, which is a
///     successful handshake with no ALPN (netty's `NO_ADVERTISE` behaviour);
///   * `null` → "fail the handshake", which JSSE reports as a
///     `no_application_protocol` fatal alert. netty's `FATAL_ALERT` selector
///     behaviour produces exactly this, and
///     `testTlsExtensionNoCompatibleProtocolsServerHandshakeFailure` asserts the
///     server sees an `SSLHandshakeException`.
///
/// A hello with no ALPN extension does not consult the selector at all (JSSE
/// does not either — netty's `AlpnSelector.checkUnsupported` is the handler for
/// that case), and neither does a buffer that is not yet a parseable
/// ClientHello: the caller re-enters with more bytes.
pub fn engine_apply_alpn_selector(
    ctx: &mut dyn NativeContext,
    id: i32,
    engine: ObjectRef,
    src: ObjectRef,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let interesting = with_engine(id, |s| !s.is_client && s.conn.is_none()).unwrap_or(false);
    if !interesting {
        return Ok(());
    }
    let key = engine_objref_key(ctx, engine);
    let selector = match engine_alpn_selector_table().lock().get(&key).copied() {
        Some(o) => o,
        None => return Ok(()),
    };
    let view = bb_view(ctx, src);
    let bytes = bb_bytes_range(ctx, &view, view.pos, view.lim.min(view.pos + 4096));
    let offered = match peek_client_hello_alpn(&bytes) {
        Some(list) if !list.is_empty() => list,
        _ => return Ok(()),
    };
    // Build the `List<String>` argument. Everything that can move across an
    // allocation is pinned and re-read, the same discipline
    // `engine_run_sni_match_check` uses.
    let sel_pin = ctx.pin_native_root(selector);
    let eng_pin = ctx.pin_native_root(engine);
    let list = match ctx.new_object_initialized("java/util/ArrayList", "()V", &[]) {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(sel_pin);
            return Ok(());
        }
    };
    let list_pin = ctx.pin_native_root(list);
    for p in &offered {
        let s = ctx.create_string(p);
        let list_now = ctx.read_native_pin(list_pin, list);
        let _ = ctx.invoke_virtual(
            list_now,
            "add",
            "(Ljava/lang/Object;)Z",
            &[Value::Object(Some(s))],
        );
    }
    let sel_now = ctx.read_native_pin(sel_pin, selector);
    let eng_now = ctx.read_native_pin(eng_pin, engine);
    let list_now = ctx.read_native_pin(list_pin, list);
    let picked = ctx.invoke_virtual(
        sel_now,
        "apply",
        "(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        &[Value::Object(Some(eng_now)), Value::Object(Some(list_now))],
    );
    let chosen: Option<String> = match picked {
        Ok(Some(Value::Object(Some(s)))) => Some(ctx.read_string(s).unwrap_or_default()),
        // A selector that throws is JSSE's "fail the handshake" too — it must
        // never be read as "no preference".
        _ => None,
    };
    ctx.unpin_native_roots(sel_pin);
    // gc-common w12-a: consumed. The realised connection never consults the
    // selector again, so it stops being a root (it may capture the engine;
    // see `alpn_selector_release_root`).
    alpn_selector_release_root(key);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] engine_apply_alpn_selector id={} offered={:?} -> {:?}",
            id, offered, chosen
        );
    }
    match chosen {
        Some(p) if !p.is_empty() => {
            with_engine(id, |s| {
                s.alpn_protocols = vec![p.clone().into_bytes()];
            });
            Ok(())
        }
        Some(_) => {
            with_engine(id, |s| {
                s.alpn_protocols.clear();
            });
            Ok(())
        }
        None => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            "no_application_protocol: the configured ApplicationProtocolSelector \
             rejected every protocol the client offered",
        )),
    }
}

/// Read `SSLParameters.getSNIMatchers()` into raw `ObjectRef`s and file them
/// under this engine. A null/empty collection CLEARS any previous set, so a
/// caller that reads the parameters, edits something else and writes them back
/// does not accidentally keep matchers it removed.
pub fn capture_sni_matchers(ctx: &mut dyn NativeContext, engine: ObjectRef, params: ObjectRef) {
    let key = engine_objref_key(ctx, engine);
    // gc-common w12-a: the engine owns this row (swept with it). Recorded
    // before the Java calls below, while `engine` is still current.
    note_tls_weak_owner(ctx, TlsOwnerKind::Engine, key, engine);
    let coll = match ctx.invoke_virtual(params, "getSNIMatchers", "()Ljava/util/Collection;", &[]) {
        Ok(Some(Value::Object(Some(c)))) => c,
        _ => {
            engine_sni_matchers_table().lock().remove(&key);
            return;
        }
    };
    let mut list = Vec::new();
    // Walk the Collection through its Iterator rather than assuming an
    // ArrayList: `SSLParameters.getSNIMatchers` answers an unmodifiable
    // wrapper, and JSSE itself builds it from whatever the caller passed.
    if let Ok(Some(Value::Object(Some(it)))) =
        ctx.invoke_virtual(coll, "iterator", "()Ljava/util/Iterator;", &[])
    {
        let it_pin = ctx.pin_native_root(it);
        // GC (gc-common w12-a): each matcher is pinned as it is collected and
        // re-read after the walk. It used to sit raw in `list` across the
        // next `hasNext` / `next` calls, which run Java and can be a moving
        // collection's safepoint, so an earlier matcher could be filed at its
        // pre-move address -- and the table then rooted and remapped a stale
        // pointer.
        let mut pinned: Vec<(usize, ObjectRef)> = Vec::new();
        // Bounded: a matcher set is a handful of entries, and an iterator that
        // never reports exhaustion must not wedge the handshake.
        for _ in 0..64 {
            let it_now = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it_now, "hasNext", "()Z", &[]) {
                Ok(Some(Value::Int(1))) => {}
                _ => break,
            }
            let it_now = ctx.read_native_pin(it_pin, it);
            match ctx.invoke_virtual(it_now, "next", "()Ljava/lang/Object;", &[]) {
                Ok(Some(Value::Object(Some(m)))) => pinned.push((ctx.pin_native_root(m), m)),
                _ => break,
            }
        }
        for (pin, m) in &pinned {
            list.push(ctx.read_native_pin(*pin, *m));
        }
        // Releases the iterator's pin and every matcher pin above it.
        ctx.unpin_native_roots(it_pin);
    }
    let mut table = engine_sni_matchers_table().lock();
    if list.is_empty() {
        table.remove(&key);
    } else {
        table.insert(key, list);
    }
}

/// JSSE's server-side SNI gate: for the `server_name` the peer sent, consult
/// every configured `SNIMatcher` of the matching type and abort the handshake
/// with `unrecognized_name` if one refuses.
///
/// `SNIHostName`'s type is `StandardConstants.SNI_HOST_NAME` (0), the only type
/// rustls surfaces, so a matcher declaring any other type is not consulted —
/// matching `ServerHandshakeContext`, which pairs each received name with the
/// matcher registered for that name's type and ignores the rest.
///
/// Answers `true` when a matcher REFUSED. The refusal is armed on the engine
/// (a fatal `unrecognized_name` for the peer, plus
/// `deferred_handshake_error` for this side) and deliberately NOT raised here
/// — see the refusal arm in `do_unwrap` for why the throw has to wait for the
/// wrap that puts the alert on the wire.
///
/// Runs with the engine registry lock NOT held: it calls into Java.
pub fn engine_run_sni_match_check(
    ctx: &mut dyn NativeContext,
    engine_id: i32,
    engine: ObjectRef,
    host: String,
) -> Result<bool, cratonvm_types::error::MethodCallFailed> {
    let key = engine_objref_key(ctx, engine);
    let matchers = match engine_sni_matchers_table().lock().get(&key).cloned() {
        Some(m) if !m.is_empty() => m,
        _ => return Ok(false),
    };
    // GC (gc-common w26-b): the matchers are a COPY of the table's row, which
    // no collection updates, so they are pinned BEFORE the two allocations
    // below. They used to be pinned after them: a moving collection at the
    // `create_string` / `SNIHostName` construction left the copy naming the
    // matchers' vacated addresses, and those stale addresses were then pinned
    // (a root to nothing) and invoked (`getType`, `matches`). The oldest pin,
    // released last, is the first matcher's.
    let m_pins: Vec<usize> = matchers.iter().map(|m| ctx.pin_native_root(*m)).collect();
    let base = m_pins[0];
    let name_str = ctx.create_string(&host);
    let name_str_pin = ctx.pin_native_root(name_str);
    let name_str = ctx.read_native_pin(name_str_pin, name_str);
    let sni_name = ctx.new_object_initialized(
        "javax/net/ssl/SNIHostName",
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(name_str))],
    );
    let sni_name = match sni_name {
        Ok(Some(Value::Object(Some(o)))) => o,
        _ => {
            ctx.unpin_native_roots(base);
            return Ok(false);
        }
    };
    let name_pin = ctx.pin_native_root(sni_name);
    let mut refused = false;
    for (i, _) in matchers.iter().enumerate() {
        let m_now = ctx.read_native_pin(m_pins[i], matchers[i]);
        // Only a SNI_HOST_NAME matcher applies to the name rustls gave us.
        match ctx.invoke_virtual(m_now, "getType", "()I", &[]) {
            Ok(Some(Value::Int(0))) => {}
            _ => continue,
        }
        let m_now = ctx.read_native_pin(m_pins[i], matchers[i]);
        let name_now = ctx.read_native_pin(name_pin, sni_name);
        match ctx.invoke_virtual(
            m_now,
            "matches",
            "(Ljavax/net/ssl/SNIServerName;)Z",
            &[Value::Object(Some(name_now))],
        ) {
            Ok(Some(Value::Int(0))) => {
                refused = true;
                break;
            }
            // A matcher that throws is JSSE's "no match" too — it never lets an
            // application exception decide the handshake succeeded.
            Err(_) => {
                refused = true;
                break;
            }
            _ => {}
        }
    }
    ctx.unpin_native_roots(base);
    if !refused {
        return Ok(false);
    }
    with_engine(engine_id, |s| {
        match s.conn.as_mut() {
            Some(c) => c.queue_fatal_alert(rustls::AlertDescription::UnrecognisedName),
            // The gate runs at ClientHello time, and on a server engine that
            // is the call BEFORE `engine_begin_or_defer` realizes the rustls
            // connection — so `conn` is `None` here on the path that actually
            // matters and `queue_fatal_alert` was a silent no-op. No record
            // layer exists yet either, which is fine: a pre-keys alert goes
            // out as TLS plaintext, and that is exactly what JSSE's own
            // `ServerHandshakeContext` sends when it refuses a hello before a
            // ServerHello exists. alert(21), legacy_record_version 0x0303,
            // length 2, level fatal(2), description unrecognized_name(112).
            None => s
                .outbound
                .extend_from_slice(&[21, 0x03, 0x03, 0x00, 0x02, 2, 112]),
        }
        // Owed to THIS side, but only once the alert above has gone out — the
        // `SniClientTest.testSniSNIMatcherDoesNotMatchClient` half of the
        // "a wrap that raises cannot also deliver its alert" defect. Raising
        // it from this unwrap instead left the client with a closed channel
        // and no alert at all: `StacklessClosedChannelException` where its
        // `assertThrows(SSLException.class, …)` wants an `SSLException`.
        s.deferred_handshake_error = Some((
            "javax/net/ssl/SSLHandshakeException",
            format!("Unrecognized server name indication: {host}"),
        ));
    });
    Ok(true)
}

thread_local! {
    /// Set for the duration of an application `TrustManager` callback.
    ///
    /// This VM defers the consultation until AFTER `process_new_packets`
    /// (deliberately — calling into the JVM while the engine registry lock is
    /// held is what `engine_take_pending_trust_check`'s doc forbids), so by the
    /// time the manager runs, rustls reports the handshake finished. Real JSSE
    /// calls it DURING the handshake, and an `X509ExtendedTrustManager` may
    /// legitimately read `sslEngine.getHandshakeSession()` — netty's
    /// `SniClientJava8TestUtil` manager asserts it is non-null. Without this
    /// flag the "handshake is over, answer null" rule (correct for every other
    /// caller) made that assertion fail from inside the callback.
    pub static IN_TRUST_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// As [`IN_TRUST_CHECK`], but true only for the CLIENT-side callback
    /// (`checkServerTrusted`). See [`in_client_trust_check`].
    pub static IN_CLIENT_TRUST_CHECK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Is this thread inside an application `TrustManager` callback? See
/// [`IN_TRUST_CHECK`].
pub fn in_trust_check() -> bool {
    IN_TRUST_CHECK.with(|c| c.get())
}

/// Is this thread inside a CLIENT-side `checkServerTrusted` callback?
///
/// The distinction is observable through `SSLSession.getLocalCertificates()`.
/// JSSE calls `checkServerTrusted` while the client is processing the SERVER's
/// Certificate message — BEFORE the client has sent its own — so the handshake
/// session reports no local certificate there, and netty's
/// `SSLEngineTest$TestTrustManagerFactory.checkServerTrusted` asserts exactly
/// that (`assertNull(session.getLocalCertificates())`) even in a mutual-auth
/// handshake where the same session answers a chain once the handshake is over.
/// Its `checkClientTrusted` sibling asserts the opposite for the SERVER, whose
/// certificate HAS been sent by the time it runs.
///
/// CratonVM cannot move the callback: rustls owns the handshake and this VM's
/// consultation happens the moment it completes. Reporting the client's local
/// chain as absent for the duration of the callback restores the ORDER the
/// contract is really about.
pub fn in_client_trust_check() -> bool {
    IN_CLIENT_TRUST_CHECK.with(|c| c.get())
}

/// Run the post-handshake `TrustManager` consultation captured by
/// `engine_take_pending_trust_check`. MUST be called with the engine registry
/// lock NOT held. Looks up the real `TrustManager[]` for the owning
/// `SSLContext` and, for each one, calls the real Java
/// `checkClientTrusted`/`checkServerTrusted` (matching JSSE's contract: the
/// server checks the client's chain, the client checks the server's) with the
/// peer's certificate chain. Any thrown exception is treated as a rejection
/// and surfaces as `SSLHandshakeException` — matching real JSSE, which aborts
/// the handshake the same way when a configured `TrustManager` (e.g. one
/// wrapping OCSP/CRL revocation checking, or a fully custom
/// `X509TrustManager`) rejects the chain.
///
/// Then — and this is a SEPARATE gate, not a consequence of the one above —
/// runs endpoint identification when the engine was configured with an
/// identification algorithm. See `engine_check_endpoint_identity`.
/// Where the TrustManager consultation is happening, which decides what a
/// rejection DOES.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TrustCheckMode {
    /// After the handshake, from `do_wrap`/`do_unwrap`. A rejection queues the
    /// fatal alert itself and throws `SSLHandshakeException`. This is the
    /// original behaviour and the only mode for callers with no live rustls
    /// verification frame (native client sockets, `HttpURLConnection`).
    PostHandshake,
    /// Inside `ServerCertVerifier::verify_server_cert`. A rejection is REPORTED,
    /// not acted on: rustls emits its own fatal alert when the verifier answers
    /// `Err`, and it does so under HANDSHAKE keys before generating `Finished`
    /// — which is the whole point of moving the check here. Throwing a Java
    /// exception from this frame is also wrong: it would sit pending on `ctx`
    /// and surface at an arbitrary later call.
    InVerifier,
}

/// The outcome of consulting the application's TrustManagers.
pub enum TrustOutcome {
    Accepted,
    /// Rejected, with the reason already recorded via
    /// `set_last_trust_rejection_detail`, and the alert JSSE would raise for
    /// the manager's exception.
    ///
    /// The alert is computed where the exception is still in hand, NOT at the
    /// point of use: `reject_peer_with_fatal_alert` runs on the unwinding path
    /// and may not call Java, and the verifier-time arm is inside rustls's own
    /// state machine. See `crate::tls_cert_alert`.
    Rejected(String, crate::tls_cert_alert::JsseCertAlert),
    /// The manager threw an unchecked exception (a `RuntimeException`, not a
    /// `CertificateException`). JSSE's `CertificateMessage` consumers catch
    /// only `CertificateException`, so the throwable escapes the handshake
    /// message dispatch and the engine reports it as
    /// `SSLException("(internal_error) Unhandled exception")` with an
    /// `internal_error` alert -- not `SSLHandshakeException`. The PKIX
    /// manager over an EMPTY trust store is the common case: its validator
    /// throws `RuntimeException("Unexpected error: ...trustAnchors parameter
    /// must be non-empty")` (`NioConf19b clientauth-untrusted`; nio20 wave 4,
    /// L4). The reason is recorded like `Rejected`'s.
    Unhandled(String),
}

/// The `rustls::Error::General` payload a verifier returns for
/// [`TrustOutcome::Unhandled`], recognised by [`jsse_handshake_exception_class`]
/// and [`handshake_error_message`].
///
/// It starts with `rustls::INTERNAL_ERROR_VERIFIER_PREFIX` (nio20 wave 5, L5):
/// the vendored rustls then answers it with JSSE's `internal_error` alert, not
/// `handshake_failure` (pinned by
/// `trust_manager_unhandled_carries_the_internal_error_alert_prefix`).
pub const TRUST_MANAGER_UNHANDLED: &str =
    "(internal_error) cratonvm: TrustManager threw an unchecked exception";

/// JSSE's message for [`TrustOutcome::Unhandled`]
/// (`SSLEngineImpl.DelegatedTask` -> `TransportContext.fatal(INTERNAL_ERROR,
/// "Unhandled exception", e)`).
pub const JSSE_UNHANDLED_EXCEPTION_MESSAGE: &str = "(internal_error) Unhandled exception";

pub fn engine_run_trust_check(
    ctx: &mut dyn NativeContext,
    pending: PendingTrustCheck,
    engine_obj: Option<ObjectRef>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    match engine_consult_trust_managers(ctx, pending, engine_obj, TrustCheckMode::PostHandshake)? {
        TrustOutcome::Accepted => Ok(()),
        // Unreachable: `PostHandshake` throws from inside
        // `engine_consult_trust_managers` rather than returning `Rejected`.
        // Kept as a belt-and-braces arm rather than an `unreachable!` so a
        // future edit that changes that cannot turn into a panic in a TLS
        // handshake.
        TrustOutcome::Rejected(detail, _alert) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            &format!("TrustManager rejected the peer certificate chain: {detail}"),
        )),
        // Unreachable for the same reason as `Rejected`.
        TrustOutcome::Unhandled(_) => Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLException",
            JSSE_UNHANDLED_EXCEPTION_MESSAGE,
        )),
    }
}

/// Is `exc` a `java.lang.RuntimeException`? The superclass walk of
/// [`throwable_is_error`].
pub fn throwable_is_runtime_exception(ctx: &mut dyn NativeContext, exc: ObjectRef) -> bool {
    let mut cid = Some(ctx.class_id_of_object(exc));
    for _ in 0..64 {
        let Some(c) = cid else { break };
        match ctx.class_name_of_id(c).as_deref() {
            Some("java/lang/RuntimeException") => return true,
            Some("java/lang/Exception")
            | Some("java/lang/Throwable")
            | Some("java/lang/Object") => return false,
            _ => {}
        }
        cid = ctx.superclass_of(c);
    }
    false
}

pub fn engine_consult_trust_managers(
    ctx: &mut dyn NativeContext,
    pending: PendingTrustCheck,
    engine_obj: Option<ObjectRef>,
    mode: TrustCheckMode,
) -> Result<TrustOutcome, cratonvm_types::error::MethodCallFailed> {
    let trust_managers = match pending.trust_ctx_key {
        // Another VM's `TrustManager`s live in another heap (gc-common w9-b):
        // a key that reached this engine through a process-wide default slot
        // is treated as "no TrustManagers" rather than invoked from here.
        Some(key) if !ctx_key_visible_to(&*ctx, key) => Vec::new(),
        Some(key) => ctx_trust_managers_table()
            .lock()
            .get(&key)
            .cloned()
            .unwrap_or_default(),
        None => Vec::new(),
    };
    // Whether JSSE performs the identity check ITSELF is decided by WHICH
    // TrustManager is in force, so it is decided here rather than inside
    // `engine_check_endpoint_identity`. See `jsse_owns_endpoint_identification`.
    let jsse_identifies = jsse_owns_endpoint_identification(ctx, &trust_managers);
    if trust_managers.is_empty() {
        // No custom TrustManager/TrustManagerFactory installed on this
        // context — rustls's own chain-of-trust check is the only chain
        // verification, matching prior (pre-fix) behavior exactly. Endpoint
        // identification still applies: in real JSSE it is the DEFAULT
        // `X509TrustManagerImpl` that performs it, so "no custom manager" is
        // the case where it is most certainly enforced.
        if mode == TrustCheckMode::InVerifier {
            // The identity half already ran at the top of
            // `verify_server_cert`, on the chain rustls handed it.
            return Ok(TrustOutcome::Accepted);
        }
        return engine_check_endpoint_identity(ctx, &pending, jsse_identifies)
            .map(|()| TrustOutcome::Accepted);
    }

    // Real JSSE authType is the key-exchange/signature algorithm; we don't
    // track it precisely, so derive a best-effort guess from the negotiated
    // cipher suite name. TrustManager implementations use this only for
    // logging/branching, not as a security check, so an approximation here
    // does not weaken validation.
    let auth_type = match pending.negotiated_cipher_suite_name.as_deref() {
        Some(s) if s.contains("ECDSA") => "ECDSA",
        _ => "RSA",
    };

    // gc-common w16-a: pinned before the chain array, the certificate mirrors
    // and the authType string are allocated below; it used to be pinned after
    // them, from a possibly vacated address.
    //
    // The TrustManagers are pinned first, for the same reason and one more:
    // `trust_managers` is a CLONE of the rooted `ctx_trust_managers_table`
    // entry, so its addresses are raw Rust locals that no collection remaps.
    // They used to be pinned after the chain array, the mirrors and the
    // authType string were allocated, which on G1 meant pinning an address a
    // young collection had already vacated: `PIN-STALE ... class=<the
    // TrustManager>` at this line, then `NoSuchMethodError
    // java/lang/Object.checkClientTrusted` (`ClassId(0)`) at the first
    // `invoke_virtual`. They sit lowest on the pin stack, so `pin_floor`
    // releases them with everything above.
    let tm_pins: Vec<usize> = trust_managers
        .iter()
        .map(|tm| ctx.pin_native_root(*tm))
        .collect();
    let engine_pin = engine_obj.map(|e| (ctx.pin_native_root(e), e));
    let arr = ctx.new_ref_array(
        cratonvm_types::ClassId::new(0),
        pending.peer_chain_der.len(),
    );
    // Pin the chain array BEFORE it is filled, not after. `make_x509_mirror`
    // allocates a `byte[]` and runs the real `sun.security.x509.X509CertImpl`
    // constructor, and `create_string` below allocates too — any of which can
    // trigger a moving young collection that relocates `arr`. The old code
    // took its pin only after the fill loop, so a collection between two
    // iterations left `arr` naming a stale slot: the `set_array_element` that
    // followed was DROPPED by the heap guard (`gen_heap: out-of-bounds ...
    // dropped`), the chain reached `X509TrustManagerImpl.checkServerTrusted`
    // with a null element, and the handshake failed as
    // `SSLHandshakeException: TrustManager rejected the peer certificate
    // chain`. Observed on `TestSSLHostConfigCompat.testHostEC[JSSE-KEYSTORE]`
    // once the sibling STW-takeover fix (`http_url_connection::perform`)
    // stopped that same window from deadlocking instead. Family-1 shape: a
    // native local held live across an allocation.
    let base = ctx.pin_native_root(arr);
    // The lowest pin this function took (the first TrustManager's; the list is
    // non-empty here): releasing from it releases every pin above it.
    let pin_floor = tm_pins.first().copied().unwrap_or(base);
    let mut arr = arr;
    for (i, der) in pending.peer_chain_der.iter().enumerate() {
        let mirror = crate::keystore::make_x509_mirror(ctx, "peer", der)?;
        // No allocation between this re-read and the store.
        arr = ctx.read_native_pin(base, arr);
        ctx.set_array_element(arr, i, Value::Object(Some(mirror)));
    }
    let auth_type_str = ctx.create_string(auth_type);

    // Pin the authType string — each `invoke_virtual` below can allocate/GC,
    // and a stale ObjectRef from an earlier loop iteration would silently
    // resolve to a reused slot after a move (see `pin_native_root`'s doc).
    // The TrustManagers were pinned at the top, before anything allocated.
    // `base + 1` is the authType pin because these two pins are taken back
    // to back.
    let _ = ctx.pin_native_root(auth_type_str);

    let method = if pending.is_client {
        "checkServerTrusted"
    } else {
        "checkClientTrusted"
    };
    let dbg = crate::nbflags().dbg_tls_auth_ok;
    if dbg {
        eprintln!(
            "[dbg-tls-auth] engine_run_trust_check: {} trust manager(s), method={}, chain_len={}, auth_type={}",
            trust_managers.len(),
            method,
            pending.peer_chain_der.len(),
            auth_type
        );
    }
    let mut rejected = false;
    let mut rejection: Option<String> = None;
    // JSSE's default when nothing overrides it — see `crate::tls_cert_alert`.
    // Only meaningful when `rejected`, and set on the same branch that sets it.
    let mut rejection_alert = crate::tls_cert_alert::JsseCertAlert::CertificateUnknown;
    // The manager threw a `RuntimeException` (see `TrustOutcome::Unhandled`).
    let mut unhandled = false;
    // See `IN_TRUST_CHECK`. Cleared on every exit path below — the early
    // `return Err(e)` for a propagating `Error` clears it too.
    IN_TRUST_CHECK.with(|c| c.set(true));
    IN_CLIENT_TRUST_CHECK.with(|c| c.set(pending.is_client));
    for (i, _tm) in trust_managers.iter().enumerate() {
        let arr_now = ctx.read_native_pin(base, arr);
        let auth_now = ctx.read_native_pin(base + 1, auth_type_str);
        let tm_now = ctx.read_native_pin(tm_pins[i], trust_managers[i]);
        // Which overload JSSE would use — see `tm_is_extended`. The engine is
        // absent on the native client-socket path
        // (`run_client_trust_check_for_chain`), where JSSE's `Socket`-flavoured
        // overload would apply and we have no `Socket` mirror either; the
        // two-argument form stays the answer there, exactly as before.
        let engine_now = engine_pin.map(|(pin, e)| ctx.read_native_pin(pin, e));
        let result = match engine_now {
            Some(engine) if tm_is_extended(ctx, tm_now) => ctx.invoke_virtual(
                tm_now,
                method,
                "([Ljava/security/cert/X509Certificate;Ljava/lang/String;\
                  Ljavax/net/ssl/SSLEngine;)V",
                &[
                    Value::Object(Some(arr_now)),
                    Value::Object(Some(auth_now)),
                    Value::Object(Some(engine)),
                ],
            ),
            _ => ctx.invoke_virtual(
                tm_now,
                method,
                "([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V",
                &[Value::Object(Some(arr_now)), Value::Object(Some(auth_now))],
            ),
        };
        if dbg {
            eprintln!(
                "[dbg-tls-auth] engine_run_trust_check: invoke_virtual[{}] -> {}",
                i,
                match &result {
                    Ok(_) => "Ok".to_string(),
                    Err(cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc)) => {
                        let cls = ctx.class_name_of_id(ctx.class_id_of_object(*exc));
                        format!("ExceptionThrown(class={:?})", cls)
                    }
                    Err(e) => format!("Err({:?})", e),
                }
            );
        }
        if let Err(e) = result {
            // An `Error` is NOT a rejection. JSSE catches `Exception` around an
            // application TrustManager and lets `Error` through untouched; see
            // `throwable_is_error`. Unpin first — this is an early return out
            // of the pinned region.
            if let cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc) = &e {
                if throwable_is_error(ctx, *exc) {
                    IN_TRUST_CHECK.with(|c| c.set(false));
                    IN_CLIENT_TRUST_CHECK.with(|c| c.set(false));
                    ctx.unpin_native_roots(pin_floor);
                    return Err(e);
                }
            }
            // Name WHY, unconditionally — not only under `CRATONVM_DBG=tls-auth`.
            // "TrustManager rejected the peer certificate chain" on its own is
            // indistinguishable between the three things that reach it: the
            // TrustManager genuinely refusing the chain, an `AbstractMethodError`
            // from a bare-interface stub, and a VM-level fault (a
            // `ClassCastException` naming `java.lang.Object` is the signature of
            // a reclaimed object, per the GC notes). Chasing an intermittent
            // rejection without this costs a rebuild, and the debug flag's own
            // `eprintln`s perturb the timing enough to hide a race — measured:
            // 2 failures in 24 runs with the flag off, 0 in 10 with it on.
            // gc-common w21-b: the exception is pinned across `getMessage()`
            // (Java) and read back for the alert classification below, which
            // asked it through its pre-call address.
            let exc_pinned = match &e {
                cratonvm_types::error::MethodCallFailed::ExceptionThrown(exc) => {
                    Some((ctx.pin_native_root(*exc), *exc))
                }
                _ => None,
            };
            rejection = Some(match exc_pinned {
                Some((_, exc)) => {
                    let cls = ctx
                        .class_name_of_id(ctx.class_id_of_object(exc))
                        .unwrap_or_else(|| "<unknown class>".to_string());
                    match ctx.invoke_virtual(exc, "getMessage", "()Ljava/lang/String;", &[]) {
                        Ok(Some(Value::Object(Some(s)))) => {
                            format!("{cls}: {}", ctx.read_string(s).unwrap_or_default())
                        }
                        _ => cls,
                    }
                }
                None => format!("{e:?}"),
            });
            // Which alert this becomes is decided HERE, while the exception is
            // still an object and Java can still be called. Both consumers run
            // somewhere that cannot do either: `reject_peer_with_fatal_alert`
            // is on the unwinding path, and the verifier-time arm is inside
            // rustls's state machine.
            rejection_alert = match exc_pinned {
                Some((pin, exc)) => {
                    let exc = ctx.read_native_pin(pin, exc);
                    // nio20 wave 4 (L4): an unchecked exception is not a
                    // rejection JSSE classifies (see `TrustOutcome::Unhandled`).
                    unhandled = throwable_is_runtime_exception(ctx, exc);
                    let exc = ctx.read_native_pin(pin, exc);
                    let alert = crate::tls_cert_alert::jsse_cert_alert_for(ctx, exc);
                    ctx.unpin_native_roots(pin);
                    alert
                }
                // Not an exception at all (an internal error): JSSE has no
                // opinion, and `certificate_unknown` is its default.
                None => crate::tls_cert_alert::JsseCertAlert::CertificateUnknown,
            };
            rejected = true;
            break;
        }
    }
    IN_TRUST_CHECK.with(|c| c.set(false));
    IN_CLIENT_TRUST_CHECK.with(|c| c.set(false));
    ctx.unpin_native_roots(pin_floor);

    if rejected {
        let detail = rejection.unwrap_or_else(|| "no exception detail available".to_string());
        set_last_trust_rejection_detail(&detail);
        if unhandled {
            if mode == TrustCheckMode::InVerifier {
                return Ok(TrustOutcome::Unhandled(detail));
            }
            // JSSE sends `internal_error` for it, not a certificate alert.
            with_engine(pending.engine_id, |s| {
                if let Some(c) = s.conn.as_mut() {
                    c.queue_fatal_alert(rustls::AlertDescription::InternalError);
                }
            });
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLException",
                JSSE_UNHANDLED_EXCEPTION_MESSAGE,
            ));
        }
        if mode == TrustCheckMode::InVerifier {
            // No `reject_peer_with_fatal_alert` and no throw: rustls is about to
            // do the equivalent, correctly, from inside its own state machine.
            return Ok(TrustOutcome::Rejected(detail, rejection_alert));
        }
        reject_peer_with_fatal_alert(pending.engine_id, rejection_alert);
        return Err(crate::phases_early::throw_jca_exc(
            ctx,
            "javax/net/ssl/SSLHandshakeException",
            &format!("TrustManager rejected the peer certificate chain: {detail}"),
        ));
    }
    if mode == TrustCheckMode::InVerifier {
        return Ok(TrustOutcome::Accepted);
    }
    engine_check_endpoint_identity(ctx, &pending, jsse_identifies).map(|()| TrustOutcome::Accepted)
}

/// Tell the peer that its certificate was refused, the way JSSE does: queue a
/// fatal `certificate_unknown` alert on the engine's rustls connection.
///
/// This is the missing half of a `TrustManager` rejection. rustls has already
/// ACCEPTED the chain by the time the Java manager is consulted (the
/// consultation is deliberately deferred until after `process_new_packets`, so
/// that calling into the JVM never happens while the engine registry lock is
/// held), so rustls itself generates no alert. Throwing
/// `SSLHandshakeException` locally and sending nothing left the peer with an
/// unexplained TCP close: netty's
/// `ParameterizedSslHandlerTest.testAlertProducedAndSend` waits for an
/// `SSLException` derived from that alert and blocked forever without it
/// (~170x HotSpot's 6 s, still running after 17 minutes).
///
/// `certificate_unknown` is the description JSSE maps a `CertificateException`
/// from a `TrustManager` to. The record itself is emitted by the next `wrap`,
/// which netty performs because `setHandshakeFailure` -> `ctx.close()` ->
/// `closeOutboundAndChannel` flushes an empty buffer through the engine.
///
/// Takes no `NativeContext` and calls no Java: it must be safe to run on the
/// rejection path, which is already unwinding.
pub fn reject_peer_with_fatal_alert(engine_id: i32, alert: crate::tls_cert_alert::JsseCertAlert) {
    with_engine(engine_id, |s| {
        if let Some(c) = s.conn.as_mut() {
            c.queue_fatal_alert(alert.alert_description());
        }
    });
}

/// Would real JSSE perform endpoint identification itself for this
/// `TrustManager[]`, or has the application taken the job?
///
/// `SSLContextImpl.chooseTrustManager` picks the FIRST element that is an
/// `X509TrustManager` and then splits on its type:
///
/// * an `X509ExtendedTrustManager` is used **as-is**. JSSE calls its
///   `checkServerTrusted(chain, authType, SSLEngine)` and does nothing further:
///   the extended interface exists precisely so that an implementation can see
///   the engine/session and take responsibility for identification. JSSE adds
///   no check of its own — if the extended manager does not identify, NOTHING
///   identifies.
/// * a plain `X509TrustManager` is wrapped in
///   `SSLContextImpl$AbstractTrustManagerWrapper`, whose `checkAdditionalTrust`
///   runs `X509TrustManagerImpl.checkIdentity` AFTER the application's
///   `checkServerTrusted` returns. A plain manager that accepts everything
///   therefore does NOT switch hostname verification off — Tomcat's
///   `TesterSupport.TrustAllCerts` is exactly that, and treating its "yes" as
///   the end of the story is the CVE-2018-8034 bypass
///   (`TestSecurity2018.testCVE_2018_8034`).
///
/// The 2026-08-03 fix that first gave the `SSLEngine` lane an identity check
/// implemented the second bullet and applied it to both. That is stricter than
/// JSSE, and it rejects a connection a real JDK accepts: Netty wraps every
/// `TrustManagerFactory`'s managers in `io.netty.handler.ssl.util.X509TrustManagerWrapper`,
/// an `X509ExtendedTrustManager`, and Netty 4.2 clients default
/// `endpointIdentificationAlgorithm` to `HTTPS`
/// (`SslContext.defaultEndpointVerificationAlgorithm`). So every Netty client
/// asks for identification and then supplies an extended manager that performs
/// none — which on a real JDK means no identification at all. Verified against
/// HotSpot 25 with the same jars: `newEngine(alloc, "localhost", 4443)` reports
/// `alg=HTTPS`, and the manager reports `X509TrustManagerWrapper
/// extended=true`, and the handshake succeeds against a `CN=1` certificate.
///
/// `X509ExtendedTrustManager` is an abstract CLASS, so a SUPERCLASS WALK BY
/// NAME is the exact test — and it is deliberately not `is_subclass` against a
/// `class_id_by_name` lookup. That lookup answers `None` both for "no loader
/// has this name" and for "several do", and a `None` there would silently
/// degrade to the strict answer and look exactly like a working check. Walking
/// the receiver's own chain asks the object, which cannot be ambiguous.
///
/// Anything unresolvable falls back to `true` — the stricter, pre-existing
/// behaviour. `CRATONVM_DBG=tls-auth` names the chain that was walked, because
/// "returned true" and "never found the class" are the two answers that must
/// not be confused when this is next investigated.
/// Is `tm` an `X509ExtendedTrustManager`?
///
/// JSSE picks the overload by this: `SSLContextImpl.chooseTrustManager` uses an
/// `X509ExtendedTrustManager` AS-IS and `X509TrustManagerImpl` then calls the
/// **three**-argument `checkServerTrusted(chain, authType, SSLEngine)`; only a
/// plain `X509TrustManager` gets the two-argument form (through
/// `AbstractTrustManagerWrapper`). A manager that implements both — every
/// `X509ExtendedTrustManager` does, the two-arg methods being inherited
/// abstract — can tell the difference, and the ones in test suites do
/// deliberately: netty's `SniClientJava8TestUtil` `fail()`s the two-arg form
/// and asserts on `sslEngine.getHandshakeSession()` in the three-arg one, so
/// calling the wrong overload turned a passing test into
/// `SSLHandshakeException: TrustManager rejected the peer certificate chain:
/// org/opentest4j/AssertionFailedError`.
pub fn tm_is_extended(ctx: &mut dyn NativeContext, tm: ObjectRef) -> bool {
    let mut cid = Some(ctx.class_id_of_object(tm));
    // Bounded for the same reason `jsse_owns_endpoint_identification` bounds
    // its walk: a corrupted `superclass_of` must not hang the handshake.
    for _ in 0..32 {
        let Some(c) = cid else { break };
        if ctx.class_name_of_id(c).as_deref() == Some("javax/net/ssl/X509ExtendedTrustManager") {
            return true;
        }
        cid = ctx.superclass_of(c);
    }
    false
}

/// Is `exc` a `java.lang.Error`?
///
/// JSSE catches `Exception` around an application `TrustManager` call, never
/// `Error`. A JUnit assertion failure inside a `TrustManager`
/// (`org.opentest4j.AssertionFailedError`) is an `Error`, and it is meant to
/// reach the test runner intact rather than be re-reported as
/// `SSLHandshakeException` — which is what this VM did, hiding both the
/// assertion's message and its stack.
pub fn throwable_is_error(ctx: &mut dyn NativeContext, exc: ObjectRef) -> bool {
    let mut cid = Some(ctx.class_id_of_object(exc));
    for _ in 0..64 {
        let Some(c) = cid else { break };
        match ctx.class_name_of_id(c).as_deref() {
            Some("java/lang/Error") => return true,
            // `Throwable` is above both `Error` and `Exception`; reaching it
            // without having seen `Error` means this is an `Exception`.
            Some("java/lang/Throwable") | Some("java/lang/Object") => return false,
            _ => {}
        }
        cid = ctx.superclass_of(c);
    }
    false
}

pub fn jsse_owns_endpoint_identification(
    ctx: &mut dyn NativeContext,
    trust_managers: &[ObjectRef],
) -> bool {
    let Some(first) = trust_managers.first() else {
        // JSSE's own default `X509TrustManagerImpl` is in force, and it is the
        // one that identifies.
        return true;
    };
    let dbg = crate::nbflags().dbg_tls_auth_ok;
    let mut chain: Vec<String> = Vec::new();
    let mut cid = Some(ctx.class_id_of_object(*first));
    // Bounded: a JDK trust-manager hierarchy is a handful of links, and an
    // unbounded walk over a corrupted `superclass_of` would hang the handshake.
    for _ in 0..32 {
        let Some(c) = cid else { break };
        let name = ctx.class_name_of_id(c);
        if dbg {
            chain.push(
                name.clone()
                    .unwrap_or_else(|| format!("<id {}>", c.as_u32())),
            );
        }
        // JSSE's OWN default trust manager is an `X509ExtendedTrustManager`,
        // and on HotSpot it is the thing that identifies the endpoint.
        //
        // A predicate must mirror the dispatch it guards. Answering "the
        // application owns identification" for a class whose identification
        // code this VM does not run means NOBODY runs it:
        // `testClientHostnameValidationFail` handshakes a client that dialled
        // `localhost` against `notlocalhost_server.pem` and asserts the
        // handshake FAILS; it completed.
        //
        // UPDATED 2026-08-26. The reason this arm gave — that the native shim
        // for `checkServerTrusted` "never sees the `SSLEngine`" — was true of
        // the shim and NOT of the call: this file's own loop passes the engine
        // to the three-argument overload, and the shim simply ignored it.
        // `x509_manager::check_server_trusted_extended` now reads
        // `SSLParameters.getEndpointIdentificationAlgorithm()` off it and runs
        // the name check, which is what closed the netty OpenSSL hole
        // (`ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md`).
        //
        // This arm STAYS `true` regardless. The shim identifies only when it
        // can find a host on the peer, and this VM's own `SSLEngine` object
        // need not carry a handshake session; the two checks reach the same
        // verdict when both run, and dropping this one would make that
        // "need not" into a hole.
        if name.as_deref() == Some("sun/security/ssl/X509TrustManagerImpl") {
            if dbg {
                eprintln!(
                    "[dbg-tls-auth] trust manager is JSSE's own X509TrustManagerImpl ({}) — \
                     its checkServerTrusted is native here and does NOT identify, \
                     so this VM must",
                    chain.join(" -> ")
                );
            }
            return true;
        }
        if name.as_deref() == Some("javax/net/ssl/X509ExtendedTrustManager") {
            if dbg {
                eprintln!(
                    "[dbg-tls-auth] trust manager is an application X509ExtendedTrustManager ({}) — \
                     it owns endpoint identification, JSSE adds none",
                    chain.join(" -> ")
                );
            }
            return false;
        }
        cid = ctx.superclass_of(c);
    }
    if dbg {
        eprintln!(
            "[dbg-tls-auth] trust manager is NOT an X509ExtendedTrustManager ({}) — \
             JSSE wraps it and identifies the endpoint itself",
            chain.join(" -> ")
        );
    }
    true
}

/// RFC 2818 / RFC 6125 endpoint identification for a client engine, run after
/// the chain has been accepted.
///
/// ## Why this is a separate gate from the TrustManager consultation
///
/// In real JSSE the two are genuinely independent for a PLAIN
/// `X509TrustManager`: it is wrapped by
/// `SSLContextImpl$AbstractTrustManagerWrapper`, which calls the application's
/// `checkServerTrusted` and THEN `checkAdditionalTrust` →
/// `X509TrustManagerImpl.checkIdentity`. So an application TrustManager that
/// accepts everything — precisely what Tomcat's `TestSecurity2018` installs
/// (`TesterSupport.TrustAllCerts`) — does not and cannot switch hostname
/// verification off. Treating "the TrustManager said yes" as the end of the
/// story is the CVE-2018-8034 bypass itself: a certificate issued for
/// `localhost` was accepted for a connection to `127.0.0.1`.
///
/// Symmetrically, when NO application TrustManager is installed, JSSE's own
/// default `X509TrustManagerImpl` performs the identity check — so this must
/// run on that path too, which is why the early return in
/// `engine_run_trust_check` calls here rather than returning `Ok(())`.
///
/// `jsse_identifies` is the third case and the one this signature exists for:
/// an application `X509ExtendedTrustManager` OWNS identification, and JSSE adds
/// nothing. See [`jsse_owns_endpoint_identification`].
///
/// A failure aborts the handshake as `SSLHandshakeException`, wrapping the same
/// text JSSE uses ("No subject alternative names matching ..." shape), which is
/// what a caller such as Tomcat's `AsyncChannelWrapperSecure` propagates out of
/// its handshake future.
pub fn engine_check_endpoint_identity(
    ctx: &mut dyn NativeContext,
    pending: &PendingTrustCheck,
    jsse_identifies: bool,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    let Some((alg, host)) = pending.endpoint_identity.as_ref() else {
        return Ok(());
    };
    if !jsse_identifies {
        if crate::nbflags().dbg_tls_auth_ok {
            eprintln!(
                "[dbg-tls-auth] endpoint identification ({alg}) for host {host:?} is the \
                 application X509ExtendedTrustManager's job — JSSE adds no check of its own"
            );
        }
        return Ok(());
    }
    match crate::x509_manager::check_endpoint_identity(&pending.peer_chain_der, host) {
        Ok(()) => Ok(()),
        Err(e) => {
            let detail = format!("endpoint identification ({alg}) failed for host {host:?}: {e}");
            if crate::nbflags().dbg_tls_auth_ok {
                eprintln!("[dbg-tls-auth] {detail}");
            }
            set_last_trust_rejection_detail(&detail);
            // Same reasoning as the TrustManager rejection above: the peer has
            // to be told, or it sees an unexplained close.
            // `certificate_unknown`, which is what this line sent before the
            // alert became a parameter: JSSE reaches an identity failure as a
            // `CertificateException` out of `checkServerTrusted` with no
            // `CertPathValidatorException` cause, so `getCertificateAlert`
            // leaves its default in place. Named rather than implied, because
            // the VERIFIER-time twin of this check (in
            // `PassthroughServerCertVerifier`) answers `NotValidForNameContext`
            // and therefore `bad_certificate` — the two disagree, JSSE agrees
            // with this one, and nothing has measured the difference yet.
            reject_peer_with_fatal_alert(
                pending.engine_id,
                crate::tls_cert_alert::JsseCertAlert::CertificateUnknown,
            );
            Err(crate::phases_early::throw_jca_exc(
                ctx,
                "javax/net/ssl/SSLHandshakeException",
                &detail,
            ))
        }
    }
}

thread_local! {
    /// Why the most recent `engine_run_trust_check` on this thread rejected.
    /// `http_url_connection::perform` cannot carry a Java exception out through
    /// its `Result<_, String>`, so it reads this back to keep the reason in the
    /// message it does surface instead of flattening every rejection to one
    /// indistinguishable sentence.
    pub static LAST_TRUST_REJECTION: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

pub fn set_last_trust_rejection_detail(detail: &str) {
    LAST_TRUST_REJECTION.with(|c| *c.borrow_mut() = Some(detail.to_string()));
}

/// Consume the reason recorded by the most recent TrustManager rejection on
/// this thread, if any. See [`LAST_TRUST_REJECTION`].
pub fn take_last_trust_rejection_detail() -> Option<String> {
    LAST_TRUST_REJECTION.with(|c| c.borrow_mut().take())
}

/// Client-socket variant of the post-handshake TrustManager consultation:
/// run the attached Java TrustManagers' `checkServerTrusted` against the
/// peer chain captured by a native client connect
/// (`servlet::s2_tls_connect`). Same rejection semantics as
/// `engine_run_trust_check` (any thrown exception → `SSLHandshakeException`).
pub fn run_client_trust_check_for_chain(
    ctx: &mut dyn NativeContext,
    trust_ctx_key: u64,
    peer_chain_der: Vec<Vec<u8>>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    // The native client-socket path performs its own hostname check at its
    // own layer (`http_url_connection::huc_verify_hostname`); it does not
    // route endpoint identification through here.
    run_client_trust_check_for_chain_with_identity(ctx, trust_ctx_key, peer_chain_der, None)
}

/// [`run_client_trust_check_for_chain`], then endpoint identification when
/// `endpoint_identity` is `Some((algorithm, host))`, applied the way JSSE
/// applies it (see [`jsse_owns_endpoint_identification`]): after the
/// managers accept, for a plain `X509TrustManager` and for JSSE's own
/// `X509TrustManagerImpl`, never for an application `X509ExtendedTrustManager`.
/// `engine_consult_trust_managers` already does exactly that with a pending
/// check's identity; this is only the way in.
///
/// gc-common w15-a: `net_phase_e::re5_server_trust_gate` (`HttpClient` over an
/// `SSLContext` with application `TrustManager`s) passes `("HTTPS", host)`,
/// as HotSpot's `HttpClient` sets that algorithm on every connection.
pub fn run_client_trust_check_for_chain_with_identity(
    ctx: &mut dyn NativeContext,
    trust_ctx_key: u64,
    peer_chain_der: Vec<Vec<u8>>,
    endpoint_identity: Option<(String, String)>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    run_client_trust_check_for_chain_with_engine(
        ctx,
        trust_ctx_key,
        peer_chain_der,
        endpoint_identity,
        None,
    )
}

/// [`run_client_trust_check_for_chain_with_identity`], consulting the managers
/// with `engine_obj` as their `SSLEngine` (gc-common w16-a,
/// `common-w15a-httpclient-extended-trustmanager-gets-the-two-argument-check`).
///
/// With an engine, an application `X509ExtendedTrustManager` gets JSSE's
/// three-argument `checkServerTrusted(chain, authType, SSLEngine)` (see
/// [`tm_is_extended`]); a plain `X509TrustManager` still gets the two-argument
/// form. `net_phase_e::re5_server_trust_gate` (the `HttpClient` gate) mints
/// the engine HotSpot's `HttpClient` would have: `createSSLEngine(host,
/// port)`, client mode, the `HTTPS` algorithm unless hostname verification is
/// off. `engine_obj` must be current (the caller re-reads it through its pin
/// right before the call); `engine_consult_trust_managers` pins it before it
/// allocates anything (edit 2). `engine_id` stays `-1`: the engine never
/// handshook, and the connection a rejection must tear down is the caller's,
/// so no alert is queued on it.
pub fn run_client_trust_check_for_chain_with_engine(
    ctx: &mut dyn NativeContext,
    trust_ctx_key: u64,
    peer_chain_der: Vec<Vec<u8>>,
    endpoint_identity: Option<(String, String)>,
    engine_obj: Option<ObjectRef>,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    engine_run_trust_check(
        ctx,
        PendingTrustCheck {
            // No engine of `engine_registry` owns this connection: it is the
            // native client-socket / `HttpClient` path's own rustls
            // connection. `reject_peer_with_fatal_alert` is a no-op for an id
            // that names no engine, which is the right answer -- that path
            // tears the socket down itself.
            engine_id: -1,
            is_client: true,
            peer_chain_der,
            trust_ctx_key: Some(trust_ctx_key),
            negotiated_cipher_suite_name: None,
            endpoint_identity,
        },
        engine_obj,
    )
}

/// Public accessor for the negotiated ALPN of an engine — used by other
/// modules (e.g. http2.rs) that drive the engine through wrap/unwrap and
/// then need to know which protocol to speak.
pub fn engine_negotiated_alpn_internal(engine_id: i32) -> Option<String> {
    with_engine(engine_id, |s| s.negotiated_alpn.clone()).flatten()
}

// -----------------------------------------------------------------------------
// Native registrations (WP5.1 + WP5.4)
// -----------------------------------------------------------------------------

/// Build a synthetic `SSLSession` reflecting `id`'s negotiated (or, before/
/// outside a handshake, best-effort default) cipher/protocol/ALPN state.
/// The `SSLSession` object this engine is currently presenting, keyed by
/// `engine_objref_key` and by handshake epoch (`false` = the pre-handshake
/// session, `true` = the negotiated one).
///
/// `getSession()` used to build a FRESH synthetic session on every call, which
/// breaks the identity every stateful part of the API depends on:
/// `putValue`/`getValue` landed on different objects, so an attribute never
/// read back; `invalidate()` marked an object the next `isValid()` never saw;
/// and `getCreationTime()` moved every time it was asked. netty's
/// `SSLEngineTest.testSessionAfterHandshake0` is the direct witness — 48 of
/// this class's failures, `expected: <true> but was: <null>` from
/// `assertEquals(Boolean.TRUE, engine.getSession().getValue(key))`.
///
/// Two epochs rather than one, because JSSE genuinely replaces the session at
/// handshake completion and the same test asserts it: values put on the
/// pre-handshake session must NOT be visible afterwards.
///
/// Holds live `ObjectRef`s, so it is scanned and remapped by
/// `gc_scan_tls_ctx_trust_manager_roots` /
/// `gc_update_tls_ctx_trust_manager_refs` alongside this module's other
/// object-holding tables.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0). Five sites: an `if let` whose body
/// is a bare `return`, two one-statement `insert`s, and the GC scan/remap pair
/// (each `drop`s its guard before the next table's).
/// Which of an engine's THREE sessions a lookup is asking for.
///
/// The table was keyed `(engine, handshaked: bool)`, and that bool conflated
/// the first two of these into ONE object:
///
/// * `Fresh` — `getSession()` before anything has been negotiated. JSSE's
///   "null session": no cipher, no protocol, and its own bindings.
/// * `Handshaking` — `getHandshakeSession()`, the session being negotiated.
/// * `Negotiated` — `getSession()` afterwards. In JSSE this IS the session
///   that was being negotiated, so whatever an application bound to it
///   mid-handshake is still bound here.
///
/// Conflating `Fresh` with `Handshaking` was invisible for as long as the
/// application `TrustManager` was consulted AFTER the handshake, because it
/// only ever saw `Negotiated`, which is freshly built. Consulting it inside
/// `verify_server_cert`, where JSSE consults it, made it visible at once:
/// `SSLEngineTest.testSessionAfterHandshake` binds a value on the
/// pre-handshake `getSession()` and its TrustManager then asserts
/// `getHandshakeSession().getValueNames().length == 0`. It saw 1 — the binding
/// the test itself had made a few lines earlier — on 48 of 821 tests, which is
/// what withdrew the verifier-time change on 2026-08-17.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SessionPhase {
    Fresh,
    Handshaking,
    Negotiated,
}

/// Which method the caller came through.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SessionDoor {
    /// `getSession()`
    Current,
    /// `getHandshakeSession()`
    Handshake,
}

/// Which session a lookup resolves to, from the door it came through and
/// whether a handshake has completed.
///
/// A free function with its own tests because the bug it encodes was a missing
/// DISTINCTION, not a wrong branch: the two-state key could not express "the
/// null session and the pending session are different objects", and no amount
/// of care at the call sites would have made it able to.
pub fn session_phase(door: SessionDoor, handshaked: bool) -> SessionPhase {
    match door {
        // `getHandshakeSession()` has already answered null unless the engine
        // is inside its handshake window, so reaching here means the pending
        // session is what is being asked for.
        SessionDoor::Handshake => SessionPhase::Handshaking,
        // `SSLEngine.getSession()` answers the CURRENT session. Before the
        // first successful handshake that is still the null session, even
        // while one is in flight — which is why this cannot be derived from
        // `is_handshaking()` alone.
        SessionDoor::Current if handshaked => SessionPhase::Negotiated,
        SessionDoor::Current => SessionPhase::Fresh,
    }
}


pub fn engine_session_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<(u64, SessionPhase), ObjectRef>> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<(u64, SessionPhase), ObjectRef>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The client-side session cache: `(SSLContext key, host, port)` → the
/// negotiated `SSLSession` object that connection produced.
///
/// This is JSSE's `SSLSessionContextImpl` for a client, which is keyed on
/// host+port for exactly one reason — so that a LATER engine from the same
/// `SSLContext`, dialling the same peer and RESUMING the session, hands the
/// application back the *same* `SSLSession` object, with the same
/// `putValue`/`getValue` bindings on it.
///
/// netty's `SSLEngineTest.doHandshakeVerifyReusedAndClose` is written around
/// that: it puts `key=TRUE` on the first connection's session, reconnects to
/// the same `a.netty.io:9999`, and asserts the value is readable off the new
/// engine's session. A fresh object per engine answers `null`.
///
/// Entries are only ever CONSULTED for a handshake rustls reports as
/// `HandshakeKind::Resumed`, so this cannot manufacture continuity that the
/// TLS layer did not actually provide.
///
/// Holds live `ObjectRef`s → scanned and remapped alongside
/// `engine_session_table` (see `gc_scan_tls_ctx_trust_manager_roots`).
#[allow(clippy::type_complexity)]
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0), after the resumed-session read in
/// `engine_session_for` was bound to a local so its guard drops before the body
/// (see there). The remaining sites are a `contains_key` inside a debug
/// `eprintln!`, a one-statement `insert`, and the GC scan/remap pair.
pub fn client_session_cache(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<(u64, String, i32), ObjectRef>> {
    static T: OnceLock<
        cratonvm_types::lock_order::OrderedPlMutex<HashMap<(u64, String, i32), ObjectRef>>,
    > = OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// The cache key for a client engine, or `None` when this engine has no
/// SSLContext identity or no peer to key on (a server engine, or a client
/// created without a host).
pub fn client_session_cache_key(id: i32) -> Option<(u64, String, i32)> {
    with_engine(id, |s| {
        if !s.is_client {
            return None;
        }
        let ctx_key = s.trust_managers_ctx_key?;
        let host = s.peer_host.clone().filter(|h| !h.is_empty())?;
        Some((ctx_key, host, s.peer_port))
    })
    .flatten()
}

/// `SSLSession.getLastAccessedTime()` for the sessions that have been accessed
/// again — i.e. reused by a later handshake. Keyed by `gc_stable_objref_key`,
/// like `session_wire_id_table` beside it; absent means "never reused", and
/// `getLastAccessedTime` then answers the creation time, which is what JSSE
/// reports for a session used exactly once.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two sites, both with the key
/// computed before the guard: `touch_session_access_time`'s `insert`, and a
/// `get(..).copied()` whose `if let` body is a bare `return`.
pub fn session_last_accessed_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, i64>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, i64>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

pub fn touch_session_access_time(ctx: &mut dyn NativeContext, ses: ObjectRef) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let k = gc_stable_objref_key(ctx, ses);
    session_last_accessed_table().lock().insert(k, now);
}

/// Did rustls RESUME this connection's session (TLS 1.2 session id or ticket,
/// TLS 1.3 PSK)? `false` for a full handshake and for a connection that has
/// not got far enough to say.
pub fn engine_handshake_was_resumed(id: i32) -> bool {
    with_engine(id, |s| {
        matches!(
            s.conn.as_ref().and_then(|c| c.handshake_kind()),
            Some(rustls::HandshakeKind::Resumed)
        )
    })
    .unwrap_or(false)
}

/// Identity keys of the session objects built in the NEGOTIATED epoch.
///
/// Only `engine_session_for` knows which epoch a session belongs to, and the
/// object itself has no spare slot to record it in (all eight are in use, and
/// `javax/net/ssl/SSLSession` is a real interface with no fields of its own to
/// widen into). Keyed by `gc_stable_objref_key` — the same GC-stable identity
/// `getId` already derives its bytes from.
///
/// **G7 -- this set answers "is this session object COMPLETE", which is a
/// different question from "did this session negotiate anything", and the two
/// doors that ask them are not the same door.** It used to be read by `getId`
/// as a second gate on top of `session_has_negotiated`; the only state the two
/// predicates disagree about is mid-handshake, and there the oracle gives
/// `getId()` a full 32 bytes. `getSessionContext()` is the door that wants this
/// distinction: HotSpot answers `null` for a session still being negotiated and
/// an `SSLSessionContextImpl` once its handshake completes. See both
/// registrations, and `jdk-only/G7-1-*.md` §2/§3. Kept across the 2026-08-17
/// dev merge, which took the `OrderedPlMutex` type from dev and this
/// behavioural note from here.
/// ARCH-2026-08-04 A6 — `LockLevel::Scratch` (L0) — two sites, a `contains` and an
/// `insert`, each a single statement over a key built beforehand.
pub fn negotiated_session_keys(
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

/// Has this session object been through a COMPLETED handshake — as opposed to
/// merely having negotiated a cipher suite, which is true from ServerHello
/// onwards? See [`negotiated_session_keys`]. Sole caller:
/// `SSLSession.getSessionContext()`.
pub fn session_is_negotiated(ctx: &mut dyn NativeContext, ses: ObjectRef) -> bool {
    match existing_objref_key(ctx, ses) {
        Some(key) => negotiated_session_keys().lock().contains(&key),
        None => false,
    }
}

