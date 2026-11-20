#[allow(unused_imports)]
use super::*;

/// Build a `CryptoProvider` restricted to `enabled` (Java cipher-suite names),
/// falling back to the unrestricted (CBC-augmented) default when `enabled` is
/// empty or maps to nothing we recognize — so an unmappable/empty list can
/// never starve the connection down to zero usable suites (which would make
/// the builder error out instead of just failing to restrict as intended).
pub fn cipher_provider_for(enabled: &[String]) -> Arc<rustls::crypto::CryptoProvider> {
    let base = cbc_augmented_default_provider();
    if enabled.is_empty() {
        return Arc::new(base);
    }
    let wanted: Vec<rustls::CipherSuite> = enabled
        .iter()
        .filter_map(|n| java_cipher_name_to_suite(n))
        .collect();
    if wanted.is_empty() {
        return Arc::new(base);
    }
    let mut restricted = base;
    // Keep the CALLER'S order, not the provider's. rustls honours the client's
    // preference order when selecting (`ServerConfig::ignore_client_order` is
    // false by default), so the order this list ends up in IS the order the
    // ClientHello offers — and therefore decides which suite gets negotiated
    // when both ends support several.
    //
    // `retain` preserved the provider's own order instead, which puts
    // AES-256 ahead of AES-128. netty's `SSLEngineTest.verifySSLSessionForMutualAuth`
    // asserts `session.getCipherSuite()` equals the suite its parameterisation
    // asked for and saw `TLS_AES_256_GCM_SHA384` where it configured
    // `TLS_AES_128_GCM_SHA256` — a restriction that was applied as a SET while
    // the caller meant it as a LIST.
    let mut ordered: Vec<rustls::SupportedCipherSuite> = Vec::new();
    for want in &wanted {
        if let Some(cs) = restricted
            .cipher_suites
            .iter()
            .find(|cs| cs.suite() == *want)
        {
            if !ordered.iter().any(|k| k.suite() == cs.suite()) {
                ordered.push(*cs);
            }
        }
    }
    restricted.cipher_suites = ordered;
    if restricted.cipher_suites.is_empty() {
        return Arc::new(cbc_augmented_default_provider());
    }
    Arc::new(restricted)
}

/// Is `name` a JSSE cipher-suite NAME (as opposed to arbitrary text)?
///
/// This is the predicate behind `SSLEngine.setEnabledCipherSuites`' contract of
/// throwing `IllegalArgumentException` for a name that is not a cipher suite.
/// It is deliberately WIDER than "a suite this engine can negotiate": JSSE
/// accepts every name in its registry, including suites that are
/// supported-but-disabled, so narrowing it to `SUPPORTED_CIPHER_SUITE_NAMES`
/// would reject configurations real JSSE accepts. The `TLS_`/`SSL_` prefix is
/// what every standard JSSE suite name carries (see the Standard Algorithm
/// Names spec), which is enough to separate a real name from `InvalidCipher` /
/// `SOME_INVALID_CIPHER` — the shapes the netty suite actually asserts on.
/// The `TLS_`/`SSL_` prefix ALONE is not enough, and that gap is what
/// `L6TlsParamSweep` row 94 measured: `TLS_NO_SUCH_SUITE` carries the prefix,
/// so it was accepted where HotSpot throws. Every real JSSE suite name is one
/// of two shapes — a TLS 1.3 name (`TLS_AES_128_GCM_SHA256` and its four
/// siblings, all of which are in `SUPPORTED_CIPHER_SUITE_NAMES`) or a
/// pre-1.3 name, which always spells out the key exchange and the cipher
/// either side of `_WITH_`. That structural test rejects a typo without
/// narrowing the set to what this engine can negotiate.
pub fn is_cipher_suite_name(name: &str) -> bool {
    java_cipher_name_to_suite(name).is_some()
        || SUPPORTED_CIPHER_SUITE_NAMES.contains(&name)
        || ((name.starts_with("TLS_") || name.starts_with("SSL_")) && name.contains("_WITH_"))
        // `TLS_EMPTY_RENEGOTIATION_INFO_SCSV` and `TLS_FALLBACK_SCSV` are
        // signalling values, not suites, and carry neither `_WITH_` nor a
        // rustls mapping — JSSE still accepts them by name.
        || name.ends_with("_SCSV")
}

/// True if at least one of `ciphers` maps to a real rustls `CipherSuite` (see
/// `java_cipher_name_to_suite`). Callers that would otherwise silently fall
/// back to an unrestricted connection (see `cipher_provider_for`) should use
/// this to detect that case up front and leave an existing connection alone
/// instead of tearing it down for a restriction that cannot actually be
/// enforced through rustls.
pub fn any_cipher_mappable(ciphers: &[String]) -> bool {
    ciphers
        .iter()
        .any(|n| java_cipher_name_to_suite(n).is_some())
}

/// Map Java protocol names (`SSLSocket`/`SSLEngine.setEnabledProtocols`,
/// Tomcat's `SSLHostConfig.protocols`) onto the rustls
/// `SupportedProtocolVersion`s to offer/accept.
///
/// Returns an EMPTY vec — meaning "leave rustls's safe defaults alone" — when
/// `enabled` is empty or names nothing rustls implements. rustls only ships
/// TLS 1.2 and TLS 1.3; the legacy names (`SSLv2Hello`/`SSLv3`/`TLSv1`/
/// `TLSv1.1`) are deliberately unmapped, exactly as Tomcat's own
/// `SSLUtilBase.getEnabled` already reports them as skipped for this engine.
/// Never returning a *narrower-than-requested* non-empty list matters: a list
/// that resolved to zero versions would make `with_protocol_versions` fail
/// and take down a connection the caller only meant to constrain.
pub fn protocol_versions_for(enabled: &[String]) -> Vec<&'static rustls::SupportedProtocolVersion> {
    if enabled.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&'static rustls::SupportedProtocolVersion> = Vec::new();
    // Preserve rustls's own preference order (1.3 before 1.2) rather than the
    // caller's, which is a preference list, not a policy ordering.
    if enabled.iter().any(|p| p == "TLSv1.3") {
        out.push(&rustls::version::TLS13);
    }
    if enabled.iter().any(|p| p == "TLSv1.2") {
        out.push(&rustls::version::TLS12);
    }
    out
}

/// True when `protocols` is a GENUINE narrowing of what this TLS stack can
/// negotiate — i.e. it names at least one version we implement and leaves at
/// least one out.
///
/// The mirror of `setEnabledCipherSuites`' "is this the full supported set?"
/// guard: callers routinely re-assert the whole list (`setEnabledProtocols(
/// getSupportedProtocols())`) as ordinary connection setup, and tearing down
/// an established connection for that would turn a harmless no-op into a
/// reconnect — and, worse, into a handshake that cannot redo semantics the
/// original one had (a Java `TrustManager` that accepted a self-signed test
/// certificate). An empty or wholly unrecognised list is likewise not a
/// restriction we can act on.
pub fn protocol_restriction_is_real(protocols: &[String]) -> bool {
    let mapped = protocol_versions_for(protocols);
    !mapped.is_empty() && mapped.len() < 2
}

/// Would real JSSE put `host` in the TLS `server_name` (SNI) extension?
///
/// FIX (tls-handshake-enforcement-gap, doc 21). The JDK only sends SNI for a
/// host name it can turn into an `SNIHostName`, and
/// `sun.security.ssl.Utilities.rawToSNIHostName` rejects anything that is not
/// a fully-qualified DNS name: IP literals (RFC 6066 forbids them outright)
/// and — the case that matters here — single-label names with no dot, such as
/// the ubiquitous `localhost`. rustls has no such rule and always advertises
/// SNI for any name that parses as a `DnsName`.
///
/// That difference is directly observable to a server. `TestSsl.testSni`'s
/// final assertion issues a plain `getUrl("https://localhost:<port>/...")`
/// against a connector with a `localhost` virtual host and
/// `defaultSSLHostConfigName="_default_"`, and expects **400**: with no SNI,
/// Tomcat's `AbstractEndpoint.checkSni` compares the DEFAULT host config
/// against the `localhost` one, they differ, and the request is rejected.
/// Sending SNI made both lookups resolve to the same `localhost` config, so
/// CratonVM answered 200 — the "SNI mismatch accepted" symptom this doc
/// originally recorded, which turns out not to be a validation gap at all but
/// an over-eager client extension.
///
/// Deliberately conservative: only suppress SNI where the JDK definitely
/// would (no dot, or a numeric/IPv6 literal). A name with a dot is passed
/// through unchanged, so ordinary internet hosts are unaffected.
pub fn jsse_would_send_sni(host: &str) -> bool {
    if host.is_empty() || !host.contains('.') || host.contains(':') || host.ends_with('.') {
        // No dot => single-label name (`localhost`); a colon => IPv6 literal.
        return false;
    }
    // Dotted-quad IPv4 literal: every label is numeric.
    if host
        .split('.')
        .all(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
    {
        return false;
    }
    matches!(
        ServerName::try_from(host.to_string()),
        Ok(ServerName::DnsName(_))
    )
}

/// Return `config` with the SNI extension suppressed when JSSE would not send
/// it for `host` (see [`jsse_would_send_sni`]). Clones only in that case, so
/// the shared, session-cache-owning config is reused for every ordinary host.
///
/// Applied on the native `HttpURLConnection` path only. The
/// `SSLSocketFactory.createSocket` path deliberately keeps sending SNI: a real
/// JSSE `SSLSocket` defers its handshake until the first I/O, so a caller can
/// still force SNI afterwards with `SSLParameters.setServerNames` (which
/// `TestSsl.testSni` does, precisely because JSSE would not send it for
/// `localhost` on its own). Ours handshakes eagerly inside `createSocket` and
/// can never see that later call, so suppressing SNI there would produce the
/// opposite wire behaviour from what such a caller asked for.
pub fn client_config_for_host(config: Arc<ClientConfig>, host: &str) -> Arc<ClientConfig> {
    if jsse_would_send_sni(host) {
        return config;
    }
    let mut cloned = (*config).clone();
    cloned.enable_sni = false;
    Arc::new(cloned)
}

/// Resolve the `(CryptoProvider, protocol versions)` pair to build a config
/// from, given a Java cipher-suite restriction and a Java protocol
/// restriction (either or both possibly empty = unrestricted).
///
/// FIX (tls-handshake-enforcement-gap, doc 21). Cipher suites and protocol
/// versions are not independent, and treating them as if they were broke
/// connections in BOTH directions:
///
/// * **Client.** A caller that narrows to TLS 1.2 suites only (Tomcat's
///   `TesterSupport.ClientSSLSocketFactory.setCipher`, and every
///   `TestSSLHostConfigCompat`/`TestSSLHostConfigCipher` case that uses it)
///   must stop OFFERING TLS 1.3 — real JSSE drops a protocol version whose
///   cipher suites are all disabled. rustls does not do that itself: it
///   happily advertises `supported_versions = [1.3, 1.2]` with a
///   `cipher_suites` list containing no TLS 1.3 suite, the peer selects
///   TLS 1.3 (its preference), finds no suite in common, and the handshake
///   dies — turning a restriction the caller expected to SUCCEED into a
///   `handshake_failure`.
/// * **Server.** The mirror image: `with_protocol_versions` hard-errors with
///   "no usable cipher suites configured" if the restricted provider has no
///   suite for any requested version, and an error there aborts
///   `engine_begin` before a single TLS byte is written, so the peer sees a
///   bare connection close. A connector configured `protocols="TLSv1.2"`
///   whose cipher list happens to resolve to TLS 1.3 suites only would take
///   the whole virtual host down. An unenforceable cipher restriction must
///   never disable a protocol version the operator explicitly configured, so
///   the restriction is dropped (and reported) rather than the version.
///
/// Precedence: an explicit protocol restriction always wins; the cipher list
/// can only narrow WITHIN it, never beyond it.
pub fn provider_and_versions(
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
) -> (
    Arc<rustls::crypto::CryptoProvider>,
    Vec<&'static rustls::SupportedProtocolVersion>,
) {
    fn versions_with_suites(
        provider: &rustls::crypto::CryptoProvider,
        candidates: &[&'static rustls::SupportedProtocolVersion],
    ) -> Vec<&'static rustls::SupportedProtocolVersion> {
        candidates
            .iter()
            .copied()
            .filter(|v| {
                provider
                    .cipher_suites
                    .iter()
                    .any(|cs| cs.version().version == v.version)
            })
            .collect()
    }

    const ALL: [&rustls::SupportedProtocolVersion; 2] =
        [&rustls::version::TLS13, &rustls::version::TLS12];
    let requested = protocol_versions_for(enabled_protocols);
    let candidates: Vec<&'static rustls::SupportedProtocolVersion> = if requested.is_empty() {
        ALL.to_vec()
    } else {
        requested
    };

    let provider = cipher_provider_for(enabled_ciphers);
    let usable = versions_with_suites(&provider, &candidates);
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] provider_and_versions ciphers={:?} protocols={:?} -> versions={:?}",
            enabled_ciphers,
            enabled_protocols,
            usable.iter().map(|v| v.version).collect::<Vec<_>>()
        );
    }
    if !usable.is_empty() {
        return (provider, usable);
    }
    // The cipher restriction starved every requested version — keep the
    // versions, drop the restriction.
    let unrestricted = Arc::new(cbc_augmented_default_provider());
    let usable = versions_with_suites(&unrestricted, &candidates);
    let versions = if usable.is_empty() {
        ALL.to_vec()
    } else {
        usable
    };
    if crate::nbflags().dbg_tls_auth_ok {
        eprintln!(
            "[dbg-tls-auth] provider_and_versions: cipher restriction {:?} has no suite for \
             protocols {:?} — restriction dropped",
            enabled_ciphers, enabled_protocols
        );
    }
    (unrestricted, versions)
}

/// A `ClientCertVerifier` that accepts any structurally-valid, correctly
/// SIGNED client certificate WITHOUT validating its chain against a trust
/// anchor.
///
/// Two callers select this:
///
/// 1. Tomcat's `trustManagerClassName` mechanism: that feature's whole point
///    is to delegate the trust decision to a Java `TrustManager` class
///    INSTEAD OF a keystore-backed truststore, so there is no CA data here
///    for `WebPkiClientVerifier` to build a `RootCertStore` from. Trust is
///    enforced afterwards, synchronously, by `engine_run_trust_check` calling
///    the real Java `TrustManager.checkClientTrusted` once the handshake
///    completes — which aborts the connection (`SSLHandshakeException`) on
///    rejection.
/// 2. Optional (`ClientAuth.WANT`/`setWantClientAuth(true)`) client auth with
///    no trust source configured at all (no truststore, no custom
///    `TrustManager`). Real JSSE does NOT fail the handshake here even when
///    the presented certificate fails trust verification against its default
///    (system cacerts) trust manager — confirmed empirically against a real
///    JDK: the handshake completes, only the SERVER's own
///    `getPeerPrincipal()`/`getPeerCertificates()` throw
///    `SSLPeerUnverifiedException` afterward. rustls's `WebPkiClientVerifier`
///    has no such soft-fail path (verification failure is always a fatal
///    alert), so this verifier is the closest achievable approximation:
///    accept the cert structurally (proves key possession via
///    `verify_tls12/13_signature`, same webpki primitives
///    `WebPkiClientVerifier` uses) without asserting CA trust. No Java
///    `TrustManager` runs afterward in this case (none is registered), so
///    unlike case 1 there is no later enforcement step — this only matters
///    for callers that read `SSLSession.getPeerCertificates()` expecting an
///    authoritative trust decision, which real JSSE would also leave
///    unresolved here (it simply drops the unverified identity instead of
///    exposing it, a difference this approximation does not fully capture).
///
/// Mandatory (`ClientAuth.NEED`/`setNeedClientAuth(true)`) client auth is
/// UNCHANGED by case 2 above and still requires a real trust source — see
/// `default_engine_server_config`'s own
/// "setNeedClientAuth(true) requires javax.net.ssl.trustStore" error.
#[derive(Debug)]
pub struct PassthroughClientCertVerifier {
    pub mandatory: bool,
    pub algorithms: rustls::crypto::WebPkiSupportedAlgorithms,
    /// Acceptable-CA list to advertise in the `CertificateRequest` — the
    /// delegating Java `TrustManager`'s `getAcceptedIssuers()`, snapshotted at
    /// `SSLContext.init` time (see `capture_accepted_issuer_dns`). Empty when
    /// no manager was registered or it accepts any issuer.
    pub root_hints: Vec<rustls::DistinguishedName>,
}

impl rustls::server::danger::ClientCertVerifier for PassthroughClientCertVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }
    fn client_auth_mandatory(&self) -> bool {
        self.mandatory
    }
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        &self.root_hints
    }
    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::server::danger::ClientCertVerified, rustls::Error> {
        Ok(rustls::server::danger::ClientCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature_lenient(message, cert, dss, &self.algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature_lenient(message, cert, dss, &self.algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// As `build_server_config_single_cert_ex_ciphers`, but requests/requires a
/// client certificate using `PassthroughClientCertVerifier` instead of
/// `WebPkiClientVerifier` — for the `trustManagerClassName` case where there
/// is no truststore-derived CA. Only call this when the caller has confirmed
/// a Java `TrustManager` is registered for the owning engine (see
/// `engine_begin`).
pub fn build_server_config_single_cert_passthrough_client_auth(
    cert_pem: &str,
    key_pem: &str,
    alpn_protocols: &[&str],
    require_client_cert: bool,
    enabled_ciphers: &[String],
    enabled_protocols: &[String],
    root_hints: Vec<rustls::DistinguishedName>,
) -> Result<Arc<ServerConfig>, String> {
    let chain = parse_cert_chain_pem(cert_pem)?;
    // See the sibling builder: JDK EC keys need the cert's public key spliced in.
    let key = repair_ec_key_for_ring(parse_private_key_pem(key_pem)?, &chain);
    let builder = server_builder_with_versions(enabled_ciphers, enabled_protocols)?;
    let algorithms = provider_and_versions(enabled_ciphers, enabled_protocols)
        .0
        .signature_verification_algorithms;
    let verifier: Arc<dyn rustls::server::danger::ClientCertVerifier> =
        Arc::new(PassthroughClientCertVerifier {
            mandatory: require_client_cert,
            algorithms,
            root_hints,
        });
    // See `identity_certified_key` for why this is not `with_single_cert`.
    let mut config = builder
        .with_client_cert_verifier(verifier)
        .with_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
            identity_certified_key(chain, key)?,
        )));
    config.alpn_protocols = alpn_protocols
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    Ok(Arc::new(config))
}

/// As `build_client_config`, but restricts the negotiable cipher suites to
/// `enabled_ciphers` (Java `SSLEngine.setEnabledCipherSuites` names) when
/// non-empty. Used by the `SSLEngine` path (`engine_begin`) so a connector's
/// configured cipher restriction is actually enforced during the handshake,
/// not just echoed back by `getEnabledCipherSuites`.
pub fn build_client_config_ciphers(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    client_auth: Option<(&str, &str)>,
    enabled_ciphers: &[String],
) -> Result<Arc<ClientConfig>, String> {
    let (provider, versions) = provider_and_versions(enabled_ciphers, &[]);
    let builder = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions)
        .map_err(|e| format!("with_protocol_versions failed: {}", e))?
        .with_root_certificates(roots);
    let mut config = match client_auth {
        Some((cert_pem, key_pem)) => {
            let chain = parse_cert_chain_pem(cert_pem)?;
            // A JDK-generated EC client identity needs the same repair as the
            // server one — ring rejects it otherwise.
            let key = repair_ec_key_for_ring(parse_private_key_pem(key_pem)?, &chain);
            // See `identity_certified_key` for why this is not
            // `with_client_auth_cert`.
            builder.with_client_cert_resolver(Arc::new(rustls::sign::SingleCertAndKey::from(
                identity_certified_key(chain, key)?,
            )))
        }
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = alpn_protocols
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    Ok(Arc::new(config))
}

// -----------------------------------------------------------------------------
// Runtime helpers — connect / accept / read / write / close
// -----------------------------------------------------------------------------

/// Connect to `host:port` using a rustls `ClientConnection`, register the
/// stream in the server registry's client-stream table, and return the id.
pub fn rustls_client_connect(
    config: Arc<ClientConfig>,
    host: &str,
    port: u16,
) -> Result<i32, String> {
    let addr = format!("{}:{}", host, port);
    // Fold IPv4-mapped destinations (`::ffff:a.b.c.d`) to plain IPv4 — on
    // Windows an AF_INET6 socket cannot reach one (WSAEADDRNOTAVAIL). `host`
    // itself is left alone: below it is the SNI name, not just a dial target.
    // See `outbound_policy::normalize_connect_addr`.
    let tcp = cratonvm_native_io::outbound_policy::connect_str_normalized(&addr)
        .map_err(|e| format!("connect {}: {}", addr, e))?;
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name {:?}: {}", host, e))?;
    let conn = ClientConnection::new(config, server_name)
        .map_err(|e| format!("ClientConnection::new failed: {}", e))?;
    let mut stream = StreamOwned::new(conn, tcp);

    // Drive the handshake to completion so the negotiated fields are
    // populated before we read them.
    //
    // FIX (client-cipher-restriction): `read_tls` returns `Ok(0)` — not an
    // `Err` — when the peer has closed the connection (rustls's documented
    // contract; its own examples all check for a zero return). This loop
    // previously ignored the return value entirely, so a server that rejects
    // the handshake and closes (e.g. no cipher suite in common — exactly
    // what a real restriction via `SSLSocket.setEnabledCipherSuites` is
    // supposed to cause) left `is_handshaking()` stuck `true` forever: every
    // subsequent `read_tls` on the already-closed socket returns `Ok(0)`
    // instantly, so the loop busy-spins at ~100% CPU rather than blocking or
    // erroring — a live-lock, not a hang-then-timeout. This was unreachable
    // before this fix (nothing previously drove a client through this path
    // to a server that legitimately rejects and closes mid-handshake), so
    // the gap was latent.
    while stream.conn.is_handshaking() {
        // A single byte read is the canonical way to pump the rustls state
        // machine across a blocking socket without reading any app data.
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("handshake write: {}", e))?;
        }
        if stream.conn.wants_read() {
            let n = stream
                .conn
                .read_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("handshake read: {}", e))?;
            if n == 0 {
                return Err(
                    "connection closed by peer during handshake (likely a rejected \
                     handshake, e.g. no cipher suite in common)"
                        .to_string(),
                );
            }
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("handshake process: {}", e))?;
        }
    }

    // E12: `"TLS"` and `"UNKNOWN"` are not JSSE vocabulary — a caller matching
    // `^TLS_` or looking the name up in the IANA registry gets an answer that
    // matches neither a real suite nor the JDK's own "nothing negotiated"
    // literal. (`"TLS"` is the worse of the two: it is the standard
    // `SSLContext.getInstance` ALGORITHM name, so it reads as legitimate.)
    // These arms sit immediately after a handshake that SUCCEEDED, so reaching
    // one is an internal inconsistency rather than an ordinary state — but the
    // value flows straight through `rustls_session_info` into `SSLSession`, so
    // it must be spelled in the vocabulary the caller reads. See
    // `phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE`.
    let negotiated_protocol = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
    }
    .to_string();
    let negotiated_cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| suite_to_java_cipher_name(cs.suite()))
        .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());

    // W7-61: registry-held duplicate, taken BEFORE `stream` moves into the
    // mutex. See `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let entry = TlsClientStreamEntry {
        stream: Arc::new(Mutex::new(stream)),
        raw,
        local_cert_chain_der: Vec::new(),
        peer_host: host.to_string(),
        peer_port: port,
        negotiated_protocol,
        negotiated_cipher,
        negotiated_alpn,
    };
    let mut reg = sreg().lock();
    let id = alloc_server_id(&mut reg);
    reg.client_streams.insert(id, entry);
    Ok(id)
}

/// Accept a TLS connection on the listener with the given id. Drives the
/// handshake to completion and stores the stream in the server-streams table.
///
/// FIX (h2-testnetutils-accept-close-deadlock): the previous version called
/// the blocking `TcpListener::accept()` *through* the `sreg()`-guarded
/// listener entry, i.e. with `sreg()`'s mutex held for the full duration of
/// the wait for a peer connection — which can be indefinite (H2's
/// `TestNetUtils.testFrequentConnections` starts a background `Task` thread
/// looping `serverSocket.accept()`, and its client-side workers can all
/// finish/bail without ever connecting, e.g. after a *different*, since-fixed
/// bug — see `bug-h2-netutils-dsa-privatekey-tls-unsupported-FIXED.md` — left them
/// throwing before they ever reached the network). The test's own `finally`
/// block then calls `serverSocket.close()` from the main thread, which needs
/// that SAME mutex (`rustls_listener_close`) to remove the listener entry —
/// permanently deadlocked against the accept thread that can never release it
/// while blocked in the kernel. Fixed by only holding `sreg()` briefly (to
/// clone the `TcpListener` handle and the `closed` flag), then polling
/// `accept()` non-blockingly outside the lock so a `close()` call can always
/// acquire the mutex immediately and is noticed within one poll interval.
/// The cipher-suite name for a connection that **did** negotiate one which
/// this VM cannot name.
///
/// E31 (E22-1's NOMINATION E): "UNKNOWN" was spelled twice, as a bare literal,
/// beside a long comment explaining why it must not become
/// `SSL_NULL_WITH_NULL_NULL`. That reasoning is right and this constant does
/// not disturb it — it gives the value a definition so the two sites cannot
/// drift and so a reader who greps the literal lands on the argument rather
/// than on a stray string. Restating the argument once, at the definition:
///
/// * The four **rustls** producer arms reach their fallback when the handshake
///   succeeded *and rustls reports no suite* — an internal inconsistency, for
///   which "nothing was negotiated" is an honest description. They use
///   `JSSE_NULL_CIPHER_SUITE`.
/// * The two **native-tls** acceptor arms are reached BY SUCCEEDING.
///   `acceptor.accept(tcp)` returned; the stream is encrypted; a suite
///   genuinely was negotiated. Writing the sentinel there would assert "no
///   cipher" about a live encrypted connection — false in the *dangerous*
///   direction, the same direction as the fabrication this family removed,
///   merely inverted. This value is not JSSE vocabulary either, but it is
///   **loud rather than plausible**, which is the correct trade when the truth
///   is unavailable. Do not "complete the family" here.
///
/// **The gap is now verified rather than asserted, and the two arms turn out
/// not to be one row.** E22-1 offered "read the suite out of the underlying
/// `SslStream` via the backend-specific escape hatch" as option (b).
///
/// * For the `Native` arm there is no such hatch in the version this tree
///   builds against. `native-tls` 0.2.18's `TlsStream` exposes exactly
///   `buffered_read_size`, `peer_certificate`, `tls_server_end_point`,
///   `negotiated_alpn` and `shutdown`; its `get_ref`/`get_mut` hand back the
///   underlying transport rather than the backend handle, and the
///   `imp::TlsStream` field is private. Even `negotiated_alpn` is
///   `#[cfg(feature = "alpn")]` and this workspace takes native-tls with
///   default features (`default = []`), so it does not exist here either.
///   Naming the suite needs a forked dependency or routing this path through
///   rustls.
/// * The `LegacyDsa` arm is **openssl**, not native-tls, and that check is
///   what separated them: `SslRef::version_str()` is unconditional and now
///   supplies the real protocol version there (it used to be a hardcoded
///   `"TLSv1.2"`). Its cipher is still unnamed only because the JSSE-spelled
///   accessor, `SslCipherRef::standard_name()`, is `#[cfg(ossl111)]`.
///
/// So this constant is reached by two arms for two different reasons, and only
/// one of them is genuinely without an accessor.
pub const NATIVE_TLS_UNNAMEABLE_SUITE: &str = "UNKNOWN";

/// The protocol-version twin of [`NATIVE_TLS_UNNAMEABLE_SUITE`], for the one
/// arm that is genuinely without an accessor: `native_tls::TlsAcceptor`.
///
/// E31-1 NOMINATION 3. The `Native` arm of [`rustls_server_accept_within`]'s
/// match used to hardcode `"TLSv1.2"` for a handshake whose real version it
/// cannot read — `native-tls` 0.2.18 exposes no version accessor on
/// `TlsStream`, unlike `openssl::ssl::SslRef::version_str()`, which is why the
/// `LegacyDsa` arm beside it could be (and was) fixed to report the real
/// value. A plausible, wrong protocol version is the same species of
/// fabrication the sibling cipher comment refuses, in the *dangerous*
/// direction: a caller testing "am I on at least TLS 1.2" would be told yes
/// for a connection that may have been negotiated at TLS 1.0 or 1.1. This
/// constant is not JSSE vocabulary either — loud rather than plausible is the
/// same trade already made for the cipher suite, and the two now use the same
/// shape.
pub const NATIVE_TLS_UNNAMEABLE_PROTOCOL: &str = "TLS_UNKNOWN_NATIVE_TLS_PROTOCOL";

/// Why [`rustls_server_accept_within`] did not return a stream.
///
/// **`TimedOut` is not a `Failed("...timed out")`.** `SSLServerSocket.accept()`
/// turns a failure into `java.io.IOException` and an expiry into
/// `java.net.SocketTimeoutException`, and an accept loop distinguishes them by
/// catching the latter and going round again — `RSslLiveSession.serve` is
/// written exactly that way. Collapsing the two into one string error would
/// make an ordinary idle tick look like a dead listener.
pub enum AcceptFailure {
    /// The socket's `SO_TIMEOUT` elapsed with no peer. Not an error state:
    /// the listener is untouched and a later `accept()` works.
    TimedOut,
    Failed(String),
}

/// Accept one TLS connection on `listener_id`, waiting at most `timeout` for a
/// peer to arrive.
///
/// `timeout` is the socket's `SO_TIMEOUT`; `None` means block indefinitely,
/// which is what `SO_TIMEOUT == 0` means in `java.net.ServerSocket` and what
/// this function did unconditionally before G25. It bounds only the wait for a
/// TCP connection — once a peer is accepted the TLS handshake runs under the
/// stream's own 30 s read/write timeouts, matching JSSE, where `SO_TIMEOUT`
/// governs `accept()` and not the handshake that follows it.
pub fn rustls_server_accept_within(
    listener_id: i32,
    timeout: Option<std::time::Duration>,
) -> Result<i32, AcceptFailure> {
    let debug_hs = crate::nbflags().dbg_tls_hs;
    // Step 1: pop the config + a cloned tcp listener handle + the closed
    // flag, then accept *without* the mutex held so long handshakes (or a
    // long wait for a peer that never connects) don't stall every other TLS
    // operation, and so `close()` is never blocked behind this wait.
    let (config, tcp_listener, closed) = {
        let reg = sreg().lock();
        let entry = reg.listeners.get(&listener_id).ok_or_else(|| {
            AcceptFailure::Failed(format!("no such SSLServerSocket id: {}", listener_id))
        })?;
        let cloned = entry
            .listener
            .try_clone()
            .map_err(|e| AcceptFailure::Failed(format!("listener try_clone failed: {e}")))?;
        (entry.config.clone(), cloned, entry.closed.clone())
    };

    tcp_listener
        .set_nonblocking(true)
        .map_err(|e| AcceptFailure::Failed(format!("listener set_nonblocking failed: {e}")))?;
    // STW-COOPERATION: this loop parks the calling thread for as long as no
    // peer connects — unboundedly, in a native, never returning to the
    // interpreter and so never reaching a safepoint poll. Left unmarked it is
    // counted as a cooperative mutator that can never cooperate, and a
    // concurrent stop-the-world pause waits on it forever: `rounds=64
    // pending=1 taken=0`, repeating, with no further progress. Same bug shape
    // and same fix as `SSLSocketInputStream.read`'s refill bracket
    // (`tomcatservletwebserverfactorytests-stw-takeover-hang-FIXED`) — that
    // pass fixed the READ on an accepted socket and left the ACCEPT itself.
    //
    // ONE region for the whole wait, not one per 20 ms tick: the body is pure
    // Rust with no Java in it, and re-entering per tick would deposit a root
    // snapshot and retire the TLAB 50 times a second for a thread that is
    // doing nothing.
    let deadline = timeout.map(|t| std::time::Instant::now() + t);
    let (tcp, _peer) = {
        let _blocked = gc_blocked_syscall();
        loop {
            match tcp_listener.accept() {
                Ok(pair) => break pair,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if closed.load(Ordering::SeqCst) {
                        return Err(AcceptFailure::Failed("listener closed".to_string()));
                    }
                    // The expiry test comes BEFORE the nap and the nap is
                    // clamped to what is left, so a 5 ms `SO_TIMEOUT` expires
                    // in about 5 ms rather than being rounded up to the 20 ms
                    // poll tick. The tick exists to keep `close()` responsive
                    // (see `closed` above), not to quantise the timeout.
                    let now = std::time::Instant::now();
                    let mut nap = std::time::Duration::from_millis(20);
                    if let Some(deadline) = deadline {
                        if now >= deadline {
                            return Err(AcceptFailure::TimedOut);
                        }
                        nap = nap.min(deadline - now);
                    }
                    std::thread::sleep(nap);
                }
                Err(e) => return Err(AcceptFailure::Failed(format!("accept failed: {}", e))),
            }
        }
    };
    let _ = tcp.set_nonblocking(false);
    if debug_hs {
        eprintln!(
            "[dbg-tls-hs] server_accept listener_id={} accepted TCP",
            listener_id
        );
    }
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));

    // A duplicate handle for the failure path below: `native_tls`'s
    // `HandshakeError::Failure` does not hand the stream back, so the only
    // reliable moment to take one is before the handshake starts.
    let tcp_dup = tcp.try_clone().ok();

    #[allow(clippy::type_complexity)]
    let handshake: Result<
        (
            TlsServerStream,
            Option<String>,
            String,
            String,
            Option<String>,
        ),
        String,
    > = (|| {
        Ok(match config {
            TlsServerConfig::Rustls(config) => {
                let conn = ServerConnection::new(config)
                    .map_err(|e| format!("ServerConnection::new failed: {e}"))?;
                let mut stream = StreamOwned::new(conn, tcp);
                while stream.conn.is_handshaking() {
                    if stream.conn.wants_read() {
                        if debug_hs {
                            eprintln!(
                                "[dbg-tls-hs] server_accept listener_id={} waiting read",
                                listener_id
                            );
                        }
                        // Blocked across the SYSCALL only. `process_new_packets`
                        // below is the one place rustls can re-enter Java, and a
                        // GC-blocked thread must not run bytecode — so the guard
                        // ends before it. The socket carries a 30s timeout, so
                        // an absent peer parks this thread for that long.
                        {
                            let _blocked = gc_blocked_syscall();
                            stream.conn.read_tls(&mut EintrIo::new(&mut stream.sock))
                        }
                        .map_err(|e| format!("server handshake read: {e}"))?;
                        stream
                            .conn
                            .process_new_packets()
                            .map_err(|e| format!("server handshake process: {e}"))?;
                    }
                    if stream.conn.wants_write() {
                        if debug_hs {
                            eprintln!(
                                "[dbg-tls-hs] server_accept listener_id={} writing response",
                                listener_id
                            );
                        }
                        {
                            let _blocked = gc_blocked_syscall();
                            stream.conn.write_tls(&mut EintrIo::new(&mut stream.sock))
                        }
                        .map_err(|e| format!("server handshake write: {e}"))?;
                    }
                }
                if debug_hs {
                    eprintln!(
                        "[dbg-tls-hs] server_accept listener_id={} handshake complete",
                        listener_id
                    );
                }
                let sni = stream.conn.server_name().map(|s| s.to_string());
                // E12: see the identical arms in `tls_connect_rustls` — the
                // "we handshaked but cannot name the outcome" spelling has to
                // be JSSE's, not this file's invention.
                let protocol = match stream.conn.protocol_version() {
                    Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
                    Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
                    _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
                }
                .to_string();
                let cipher = stream
                    .conn
                    .negotiated_cipher_suite()
                    .map(|cs| suite_to_java_cipher_name(cs.suite()))
                    .unwrap_or_else(|| {
                        crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string()
                    });
                let alpn = stream
                    .conn
                    .alpn_protocol()
                    .and_then(|b| String::from_utf8(b.to_vec()).ok());
                (TlsServerStream::Rustls(stream), sni, protocol, cipher, alpn)
            }
            TlsServerConfig::Native(acceptor) => {
                let stream = acceptor
                    .accept(tcp)
                    .map_err(|e| format!("legacy TLS server handshake: {e}"))?;
                // E12 — DELIBERATELY NOT the sentinel, and this is the row that
                // shows why a family fix applied uniformly would be wrong. The
                // two rustls arms above reach their fallback only when the
                // handshake succeeded yet reports no suite (an internal
                // inconsistency, so "nothing negotiated" is honest). THIS arm
                // reached here BY succeeding: `accept()` returned, the stream
                // is encrypted, a suite genuinely was negotiated — native-tls
                // 0.2 simply exposes no accessor to name it. Writing
                // `SSL_NULL_WITH_NULL_NULL` here would assert "no cipher" about
                // a live encrypted connection, and security-sensitive code
                // branching on that string would conclude the wrong thing in
                // the DANGEROUS direction. `"UNKNOWN"` is not JSSE vocabulary
                // either — it is loud rather than plausible, which is the
                // correct trade when the truth is unavailable. The argument now
                // lives once, on `NATIVE_TLS_UNNAMEABLE_SUITE`, together with
                // the verification that no accessor exists in native-tls
                // 0.2.18 to make it knowable.
                //
                // E31 — ALPN looked knowable here and is NOT:
                // `TlsStream::negotiated_alpn()` exists in native-tls 0.2.18
                // but is `#[cfg(feature = "alpn")]`, and this workspace depends
                // on `native-tls = "0.2"` with default features, whose
                // `default = []`. So `SSLSocket.getApplicationProtocol()`
                // answering `""` for a legacy-accepted socket is a dependency
                // feature gap, not an oversight, and reading it would not
                // compile. Recorded because "the accessor exists" was the
                // obvious next move and it is wrong.
                //
                // E31-1 NOMINATION 3 — CHANGED. The `"TLSv1.2"` beside the
                // suite used to be a fabrication of the same species the
                // comment above refuses for the cipher — a real, plausible
                // protocol version asserted about a handshake whose version is
                // equally unknowable here, and worse than a conservative
                // under-report on THESE arms specifically because they are the
                // *legacy* acceptors: a caller testing "am I on at least TLS
                // 1.2" was told yes for a connection that may have been 1.0 or
                // 1.1. `native-tls` 0.2.18 exposes no version accessor and
                // this workspace has no fork or rustls-routed replacement for
                // this acceptor, so the only accessor-free choice consistent
                // with the cipher's own trade is a loud sentinel rather than a
                // silently plausible one. `getProtocol()`'s only contract
                // (`the_null_session_string_answers_are_never_null` and its
                // siblings) is non-null, which this satisfies.
                (
                    TlsServerStream::Native(stream),
                    None,
                    NATIVE_TLS_UNNAMEABLE_PROTOCOL.to_string(),
                    NATIVE_TLS_UNNAMEABLE_SUITE.to_string(),
                    None,
                )
            }
            #[cfg(unix)]
            TlsServerConfig::LegacyDsa(acceptor) => {
                let stream = acceptor
                    .accept(tcp)
                    .map_err(|e| format!("legacy DSA TLS server handshake: {e}"))?;
                // E12: same reasoning as the `Native` arm above for the CIPHER
                // — a real handshake we cannot name is NOT "nothing
                // negotiated".
                //
                // E31 — but this arm is NOT the same as the `Native` one, and
                // treating the two as a family is what hid it. This stream is
                // `openssl::ssl::SslStream`, not `native_tls::TlsStream`, and
                // openssl DOES expose the negotiated protocol version:
                // `SslRef::version_str()` (`SSL_get_version`), unconditional in
                // openssl 0.10.76 — no `ossl111`-style cfg gate, unlike
                // `SslCipherRef::standard_name()`. So the `"TLSv1.2"` literal
                // that used to stand here was a fabrication with a real value
                // sitting one call away, and on the LEGACY acceptor of all
                // places: a caller testing "am I on at least TLS 1.2" was told
                // yes for a connection that may have been 1.0 or 1.1 — false in
                // the dangerous direction, which is exactly what the cipher
                // comment above refuses to do.
                //
                // The CIPHER is deliberately still unnamed here even though
                // `SslCipherRef::name()` is unconditional: it returns the
                // OpenSSL spelling (`ECDHE-RSA-AES256-GCM-SHA384`), not the
                // JSSE/IANA one, and `standard_name()` — which does return the
                // JSSE spelling — is `#[cfg(ossl111)]`, a build-configuration
                // gate this lane cannot verify. Handing JSSE callers an
                // OpenSSL-vocabulary name is a different wrong answer, not a
                // right one; the real fix is a name-mapping table, nominated.
                let proto = stream.ssl().version_str().to_string();
                (
                    TlsServerStream::LegacyDsa(stream),
                    None,
                    proto,
                    NATIVE_TLS_UNNAMEABLE_SUITE.to_string(),
                    None,
                )
            }
        })
    })();

    // FIX (h2-testtools-ssl-accept-loop-dies): a handshake failure is NOT an
    // `accept()` failure. It used to be raised straight out of this function,
    // which `SSLServerSocket.accept()` turns into an `IOException` — and an
    // H2 `TcpServer.listen()` loop (like any JSSE accept loop, which has no
    // reason to expect a handshake error there) exits on it, taking the whole
    // server down. `TcpServer.isRunning()` opens a loopback socket and closes
    // it again WITHOUT any I/O, so H2 kills its own SSL server on the first
    // liveness probe: the real JDBC client that follows then finds a listening
    // socket nobody is accepting from, waits out the 30 s read timeout, and
    // reports `the handshake process was interrupted` — the symptom this
    // cluster's TLS residual was filed for, and the reason it read as a
    // client-side message-mapping problem.
    //
    // What HotSpot does with the same three probes, MEASURED (`TlsProbe2`):
    // `SERVER accepted` four times, three worker threads dying on
    // `SSLHandshakeException: Remote host terminated the handshake`, and the
    // fourth connection completing its handshake and rejecting the
    // certificate in 149 ms. The listener never notices.
    let (stream, sni_hostname, negotiated_protocol, negotiated_cipher, negotiated_alpn) =
        match handshake {
            Ok(parts) => parts,
            Err(reason) => {
                if debug_hs {
                    eprintln!(
                        "[dbg-tls-hs] server_accept listener_id={} handshake FAILED, \
                         deferring to first I/O: {reason}",
                        listener_id
                    );
                }
                (
                    TlsServerStream::HandshakeFailed {
                        tcp: tcp_dup,
                        reason,
                    },
                    None,
                    // What JSSE reports for a session that never negotiated.
                    "NONE".to_string(),
                    "SSL_NULL_WITH_NULL_NULL".to_string(),
                    None,
                )
            }
        };

    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.tcp().and_then(|t| t.try_clone().ok());
    let entry = TlsServerStreamEntry {
        stream: Arc::new(Mutex::new(stream)),
        raw,
        sni_hostname,
        negotiated_protocol,
        negotiated_cipher,
        negotiated_alpn,
    };
    let mut reg = sreg().lock();
    // gc-common w14-b: an accepted stream belongs to its listener's VM. A
    // listener closed during the accept gives an unowned stream.
    let owner = reg.owners.get(&listener_id).copied();
    let id = alloc_server_id(&mut reg);
    reg.server_streams.insert(id, entry);
    if let Some(vm) = owner {
        reg.owners.insert(id, vm);
    }
    Ok(id)
}

/// Read from either a client- or server-side rustls stream.
/// Drive a rustls SERVER handshake over an already-connected `TcpStream` and
/// register the result in the client/server stream registry. This is the
/// server-mode half of the deferred handshake for
/// `SSLSocketFactory.createSocket(Socket, String, int, boolean)` — see
/// `phases_late.rs`'s registration of that method and
/// `ensure_layered_handshake_started` for why the handshake itself must not
/// run at `createSocket()` time (real JDK contract: the returned socket
/// defaults to CLIENT mode; the caller decides server mode afterward via
/// `setUseClientMode(false)`, exactly what MockWebServer does).
/// GC-safe: takes only owned, non-`ObjectRef` data — no Java object is held
/// across this call.
pub fn rustls_server_handshake_over_stream(
    tcp: TcpStream,
    cert_pem: &str,
    key_pem: &str,
) -> Result<i32, String> {
    let debug_srv = crate::nbflags().dbg_tls_srv;
    if debug_srv {
        eprintln!("[dbg-tls-srv] wrap_existing_socket: got raw stream");
    }
    let config = build_server_config_single_cert(cert_pem, key_pem, &[], false, None)?;
    let conn =
        ServerConnection::new(config).map_err(|e| format!("layered server connection: {e}"))?;
    let mut stream = StreamOwned::new(conn, tcp);
    let mut iter_n = 0u32;
    while stream.conn.is_handshaking() {
        iter_n += 1;
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] wrap_existing_socket: hs loop iter={} wants_read={} wants_write={}",
                iter_n,
                stream.conn.wants_read(),
                stream.conn.wants_write()
            );
        }
        if stream.conn.wants_read() {
            let n = stream
                .conn
                .read_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("layered server handshake read: {e}"))?;
            if debug_srv {
                eprintln!(
                    "[dbg-tls-srv] wrap_existing_socket: read_tls -> {} bytes",
                    n
                );
            }
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("layered server handshake process: {e}"))?;
        }
        if stream.conn.wants_write() {
            let n = stream
                .conn
                .write_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("layered server handshake write: {e}"))?;
            if debug_srv {
                eprintln!(
                    "[dbg-tls-srv] wrap_existing_socket: write_tls -> {} bytes",
                    n
                );
            }
        }
    }
    // The handshake-completion flight (server Finished, and — for TLS1.3 — any
    // automatically queued NewSessionTicket messages) can leave `wants_write()`
    // true on the very iteration `is_handshaking()` flips to false. The loop
    // above still drains it before exiting (wants_write is checked
    // unconditionally each iteration), but a second pass here is cheap
    // insurance against a rustls-internal ordering where post-handshake output
    // is queued only after `is_handshaking()` is observed false.
    while stream.conn.wants_write() {
        let n = stream
            .conn
            .write_tls(&mut EintrIo::new(&mut stream.sock))
            .map_err(|e| format!("layered server post-handshake write: {e}"))?;
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] wrap_existing_socket: post-hs write_tls -> {} bytes",
                n
            );
        }
        if n == 0 {
            break;
        }
    }
    if debug_srv {
        eprintln!(
            "[dbg-tls-srv] wrap_existing_socket: handshake done protocol={:?} wants_write={}",
            stream.conn.protocol_version(),
            stream.conn.wants_write()
        );
    }
    // FIX (mockwebserver-taskqueue-shutdown): this stream's reads (via
    // `rustls_stream_read`, both for the request that follows this handshake
    // and any later HTTP/1.1 keep-alive request on the same connection) had
    // no read timeout at all — unlike `rustls_server_accept`'s 30s, set
    // before ITS handshake. A caller like MockWebServer's `SocketHandler`
    // (`mockwebserver3`/`okhttp3.mockwebserver`) loops reading a next
    // request after every response to support keep-alive; if the client
    // (e.g. Reactor Netty, which pools connections rather than closing them
    // eagerly) never sends one, that read blocks forever, so the
    // connection's background task never finishes and `MockWebServer.close()`
    // — which only waits 5s per task queue for an idle signal before
    // throwing `AssertionError: Gave up waiting for queue to shut down` —
    // reliably times out. 3s: (a) short enough that even the worst-case
    // "response just sent, close() called immediately after" race (the
    // common case in these tests — there's no deliberate delay between
    // receiving the response and the test's `try`-block exit) leaves >1.5s
    // of slack inside the 5s budget for the resulting IOException to
    // propagate, get caught by `SocketHandler.handle()`'s own
    // `catch (IOException)` (logged at FINE, not rethrown), and the task to
    // signal idle; (b) generous enough to not false-trigger on genuine
    // in-flight traffic — this is a loopback socket, so read latency for
    // data the peer already sent is bounded by OS scheduling, not network
    // RTT, and every handshake+request cycle observed in this investigation
    // (`CRATONVM_DBG_TLS_SRV`-traced) completed in well under 100ms even on
    // this heavily shared, contended build host.
    let _ = stream
        .sock
        .set_read_timeout(Some(std::time::Duration::from_secs(3)));
    let sni_hostname = stream.conn.server_name().map(|s| s.to_string());
    // E12: `"TLS"` and `"UNKNOWN"` are not JSSE vocabulary — a caller matching
    // `^TLS_` or looking the name up in the IANA registry gets an answer that
    // matches neither a real suite nor the JDK's own "nothing negotiated"
    // literal. (`"TLS"` is the worse of the two: it is the standard
    // `SSLContext.getInstance` ALGORITHM name, so it reads as legitimate.)
    // These arms sit immediately after a handshake that SUCCEEDED, so reaching
    // one is an internal inconsistency rather than an ordinary state — but the
    // value flows straight through `rustls_session_info` into `SSLSession`, so
    // it must be spelled in the vocabulary the caller reads. See
    // `phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE`.
    let negotiated_protocol = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
    }
    .to_string();
    let negotiated_cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| suite_to_java_cipher_name(cs.suite()))
        .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());
    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let mut reg = sreg().lock();
    let id = alloc_server_id(&mut reg);
    reg.server_streams.insert(
        id,
        TlsServerStreamEntry {
            stream: Arc::new(Mutex::new(TlsServerStream::Rustls(stream))),
            raw,
            sni_hostname,
            negotiated_protocol,
            negotiated_cipher,
            negotiated_alpn,
        },
    );
    Ok(id)
}

/// Drive a rustls CLIENT handshake over an already-connected `TcpStream` —
/// the client-mode counterpart of `rustls_server_handshake_over_stream` for
/// the same deferred `SSLSocketFactory.createSocket(Socket, String, int,
/// boolean)` design (real JDK default mode; also the shape used for a
/// layered TLS upgrade over an existing plain socket, e.g. after an HTTP
/// CONNECT tunnel). Mirrors `rustls_client_connect` minus the `TcpStream::connect`
/// step — `tcp` is already connected to `host:port` (or tunneled to it).
pub fn rustls_client_handshake_over_stream(
    tcp: TcpStream,
    config: Arc<ClientConfig>,
    host: &str,
) -> Result<i32, String> {
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name {:?}: {}", host, e))?;
    let conn = ClientConnection::new(config, server_name)
        .map_err(|e| format!("ClientConnection::new failed: {}", e))?;
    let mut stream = StreamOwned::new(conn, tcp);
    while stream.conn.is_handshaking() {
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("handshake write: {}", e))?;
        }
        if stream.conn.wants_read() {
            let n = stream
                .conn
                .read_tls(&mut EintrIo::new(&mut stream.sock))
                .map_err(|e| format!("handshake read: {}", e))?;
            if n == 0 {
                return Err(
                    "connection closed by peer during handshake (likely a rejected \
                     handshake, e.g. no cipher suite in common)"
                        .to_string(),
                );
            }
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("handshake process: {}", e))?;
        }
    }
    // E12: `"TLS"` and `"UNKNOWN"` are not JSSE vocabulary — a caller matching
    // `^TLS_` or looking the name up in the IANA registry gets an answer that
    // matches neither a real suite nor the JDK's own "nothing negotiated"
    // literal. (`"TLS"` is the worse of the two: it is the standard
    // `SSLContext.getInstance` ALGORITHM name, so it reads as legitimate.)
    // These arms sit immediately after a handshake that SUCCEEDED, so reaching
    // one is an internal inconsistency rather than an ordinary state — but the
    // value flows straight through `rustls_session_info` into `SSLSession`, so
    // it must be spelled in the vocabulary the caller reads. See
    // `phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE`.
    let negotiated_protocol = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL,
    }
    .to_string();
    let negotiated_cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| suite_to_java_cipher_name(cs.suite()))
        .unwrap_or_else(|| crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());
    // W7-61: see `TlsClientStreamEntry::raw`.
    let raw = stream.sock.try_clone().ok();
    let entry = TlsClientStreamEntry {
        stream: Arc::new(Mutex::new(stream)),
        raw,
        local_cert_chain_der: Vec::new(),
        peer_host: host.to_string(),
        peer_port: 0,
        negotiated_protocol,
        negotiated_cipher,
        negotiated_alpn,
    };
    let mut reg = sreg().lock();
    let id = alloc_server_id(&mut reg);
    reg.client_streams.insert(id, entry);
    Ok(id)
}

// -----------------------------------------------------------------------------
// Deferred handshake for SSLSocketFactory.createSocket(Socket, String, int,
// boolean) — see phases_late.rs's registration of that method.
// -----------------------------------------------------------------------------
//
// Real JDK contract: the returned socket defaults to CLIENT mode; a caller
// may still call `setUseClientMode(false)` before `startHandshake()` (or the
// first I/O call, which implicitly starts the handshake) to flip it to
// SERVER mode — exactly what MockWebServer's HTTPS listener does. The
// handshake therefore cannot run inside `createSocket()` itself (its role
// isn't known yet); everything needed to run it later is resolved and
// stashed here instead. Resolving the client `ClientConfig`/server identity
// PEM strings eagerly (rather than holding the originating `SSLContext`
// `ObjectRef`) keeps this table GC-safe: no Java object reference is held
// across the return-to-Java-and-back window between `createSocket()` and
// whenever the handshake actually starts.
/// Whether this pending socket's underlying connection is the caller's
/// actual `wrapped` stream, or (see `stash_pending_layered_socket`'s doc)
/// a `host:port` to dial fresh because `wrapped`'s stream could not be
/// extracted.
pub enum PendingLayeredStream {
    Reused(TcpStream),
    DialFresh { host: String, port: u16 },
}

pub struct PendingLayeredSocket {
    pub stream: PendingLayeredStream,
    // Raw ingredients rather than a pre-built `ClientConfig`: real JSSE lets
    // a caller narrow the negotiable cipher suites via
    // `SSLSocket.setEnabledCipherSuites()` any time before the handshake
    // actually starts — confirmed necessary by
    // `connectWithSslBundleAndOptionsMismatch`, which relies on exactly this
    // sequence (createSocket, then setEnabledCipherSuites to a
    // deliberately-mismatched suite, then the implicit handshake) to make
    // the handshake genuinely fail. Building the `ClientConfig` eagerly in
    // `stash_pending_layered_socket` would freeze the cipher list before
    // that call had a chance to run.
    pub extra_roots: Vec<Vec<u8>>,
    pub use_java_trust_manager: bool,
    pub client_identity: Option<(String, String)>,
    pub enabled_ciphers: Vec<String>,
    /// Same deal for `SSLSocket.setEnabledProtocols()` — a caller may narrow
    /// the offered TLS versions any time before the deferred handshake
    /// starts, and (per real JSSE) the handshake must then genuinely fail
    /// when the peer supports none of them.
    pub enabled_protocols: Vec<String>,
    pub server_identity: Option<(String, String)>,
    pub host: String,
    pub client_mode: bool,
    /// gc-common w14-b: the VM that stashed this socket. The handshake's
    /// stream row is claimed for it, and its teardown drops the row if the
    /// handshake never ran (`forget_vm_rustls_sockets`).
    pub vm: usize,
    /// gc-common w16-d: the `SSLSocket`'s weak lock key (the second half of
    /// `ssl_security::ssl_sock_key`, gc-common w27-a), once the socket exists
    /// ([`set_pending_layered_socket_owner`]). Its death drops this row
    /// ([`forget_pending_layered_sockets_of`], from
    /// `ssl_security::forget_ssl_security_obj_keys`).
    pub sock_key: Option<u64>,
}

pub fn pending_layered_sockets() -> &'static Mutex<HashMap<i32, PendingLayeredSocket>> {
    static T: OnceLock<Mutex<HashMap<i32, PendingLayeredSocket>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Extract `wrapped`'s TCP stream and resolve both a client and a server
/// handshake config from `ssl_context` up front, then stash all of it
/// (defaulting to CLIENT mode, the real JDK default) under a fresh pending
/// id. Returns the RAW pending id — the caller (phases_late.rs) applies
/// `servlet::PENDING_LAYERED_SOCK_ID_BASE` the same way other id ranges here
/// are offset by their callers.
///
/// `wrapped`'s stream extraction (`take_raw_socket_stream_for_tls`) only
/// understands two socket shapes: this module's own legacy s2-registry
/// sockets, and a real-JDK `NioSocketImpl`'s `impl`/`fd` field chain (the
/// shape a real `ServerSocket.accept()` produces — MockWebServer's SERVER-mode
/// use of this API). Apache HttpComponents' client-side `wrapped` socket (a
/// plain, already-connected `Socket` from its own connection-establishment
/// path) is neither, so extraction fails there — confirmed via
/// `HttpComponentsClientHttpRequestFactoryBuilderTests`' `IOException:
/// wrapped Socket has no FileDescriptor` regression while adding SERVER-mode
/// reuse. Falling back to a fresh `host:port` dial for exactly this case
/// preserves this API's prior (already-verified, dev cluster-tested)
/// CLIENT-mode behavior, which never attempted to reuse `wrapped` at all —
/// only SERVER mode strictly requires the real stream (there is no
/// "fall back to dialing" for accepting an inbound connection), and
/// SERVER-mode callers (MockWebServer) always go through the
/// NioSocketImpl-shaped path extraction already handles.
pub fn stash_pending_layered_socket(
    ctx: &mut dyn NativeContext,
    wrapped: ObjectRef,
    ssl_context: ObjectRef,
    host: String,
    port: u16,
    extra_roots: Vec<Vec<u8>>,
    java_tm_key: Option<u64>,
) -> Result<i32, String> {
    let debug_pls = crate::nbflags().dbg_tls_pls;
    let stream = match crate::net_phase_e::take_raw_socket_stream_for_tls(ctx, wrapped) {
        Ok(tcp) => {
            if debug_pls {
                eprintln!("[dbg-tls-pls] stash: reused wrapped stream host={host} port={port}");
            }
            PendingLayeredStream::Reused(tcp)
        }
        Err(e) => {
            if debug_pls {
                eprintln!(
                    "[dbg-tls-pls] stash: extraction failed ({e}), dial-fresh host={host} port={port}"
                );
            }
            PendingLayeredStream::DialFresh {
                host: host.clone(),
                port,
            }
        }
    };
    if debug_pls {
        eprintln!(
            "[dbg-tls-pls] stash: extra_roots.len()={} java_tm_key={:?}",
            extra_roots.len(),
            java_tm_key
        );
    }
    // `extra_roots`/`java_tm_key` are the caller's own resolution
    // (`phases_late::p68_factory_trust_roots`/`p68_factory_java_tm_key`,
    // keyed off the FACTORY object, args[0] at the `createSocket` call site)
    // — NOT re-derived here from `ssl_context` alone. Confirmed necessary:
    // `t27_tls::context_trust_root_ders(ctx, ssl_context)` (the
    // context-identity-keyed table `attach_trust_managers_to_ctx` populates)
    // came up empty for an SSLBundle-configured HttpComponents scenario
    // (`connectWithSslBundle` rejecting the bundle's own self-signed cert as
    // `UnknownIssuer`) — that request's anchors instead live in the legacy
    // p68 FACTORY-identity-keyed table (`p68_ctx_trust_roots_table`),
    // populated directly from the `TrustManager[]` at `SSLContext.init()`
    // time and carried forward onto the factory by
    // `SSLContext.getSocketFactory()`. `p68_factory_trust_roots` already
    // checks both tables (factory-keyed first, context-keyed fallback), so
    // reusing its resolution here — rather than only the context-keyed half
    // — covers both paths. The actual `ClientConfig` is built later, in
    // `drive_pending_layered_handshake`, once any `setEnabledCipherSuites`
    // narrowing is known too.
    let use_java_trust_manager = java_tm_key.is_some();
    let client_identity = ctx_identity(ctx, ssl_context)
        .map_err(|_| "--real-jdk refused a class this TLS context needs".to_string())?;
    // Server identity: same resolution `rustls_server_handshake_over_stream`'s
    // former caller used (this SSLContext's own identity, else the
    // process-wide runtime-configured one) — resolved here too so SERVER mode
    // never needs to touch `ssl_context` again.
    let server_identity = match ctx_identity(ctx, ssl_context)
        .map_err(|_| "--real-jdk refused a class this TLS context needs".to_string())?
    {
        Some(identity) => Some(identity),
        None => runtime_tls_identity(ctx.vm_identity())
            .map(|identity| (identity.cert_pem, identity.key_pem)),
    };
    let vm = ctx.vm_identity();
    let mut pending = pending_layered_sockets().lock();
    let mut id = 1i32;
    while pending.contains_key(&id) {
        id = id.checked_add(1).unwrap_or(1);
    }
    pending.insert(
        id,
        PendingLayeredSocket {
            stream,
            extra_roots,
            use_java_trust_manager,
            client_identity,
            enabled_ciphers: Vec::new(),
            enabled_protocols: Vec::new(),
            server_identity,
            host,
            client_mode: true,
            vm,
            sock_key: None,
        },
    );
    Ok(id)
}

/// `SSLSocket.setEnabledCipherSuites(String[])` on a still-pending layered
/// socket. A no-op if `pending_id` is unknown, same convention as
/// `set_pending_layered_socket_client_mode`.
pub fn set_pending_layered_socket_ciphers(pending_id: i32, ciphers: Vec<String>) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.enabled_ciphers = ciphers;
    }
}

/// `setEnabledProtocols(...)` on a still-pending layered socket — the
/// protocol-version sibling of `set_pending_layered_socket_ciphers`.
pub fn set_pending_layered_socket_protocols(pending_id: i32, protocols: Vec<String>) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.enabled_protocols = protocols;
    }
}

/// `setUseClientMode(false)` on a still-pending layered socket. A no-op if
/// `pending_id` is unknown (already handshaked, or was never a pending
/// layered socket) — matches this codebase's existing convention of ignoring
/// a client-mode change once a handshake is underway/complete.
/// Record the `SSLSocket` that owns pending layered socket `pending_id`
/// (gc-common w16-d). A no-op for an unknown id.
pub fn set_pending_layered_socket_owner(pending_id: i32, sock_key: u64) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.sock_key = Some(sock_key);
    }
}

/// `SSLSocket.close()` before the handshake: drop the pending entry, which
/// closes a reused stream after the guard is released (gc-common w16-d).
/// Answers whether there was one.
pub fn drop_pending_layered_socket(pending_id: i32) -> bool {
    let dropped = pending_layered_sockets().lock().remove(&pending_id);
    let found = dropped.is_some();
    drop(dropped);
    found
}

/// The `SSLSocket` keyed `sock_key` in VM `vm` died before its handshake:
/// drop its pending entries (streams closed outside the lock) and answer how
/// many (gc-common w16-d; from `ssl_security::forget_ssl_security_obj_keys`
/// since w27-a).
pub fn forget_pending_layered_sockets_of(vm: usize, sock_key: u64) -> usize {
    let dropped: Vec<PendingLayeredSocket> = {
        let mut pending = pending_layered_sockets().lock();
        let ids: Vec<i32> = pending
            .iter()
            .filter(|(_, p)| p.vm == vm && p.sock_key == Some(sock_key))
            .map(|(id, _)| *id)
            .collect();
        ids.into_iter()
            .filter_map(|id| pending.remove(&id))
            .collect()
    };
    dropped.len()}

pub fn set_pending_layered_socket_client_mode(pending_id: i32, client_mode: bool) {
    if let Some(p) = pending_layered_sockets().lock().get_mut(&pending_id) {
        p.client_mode = client_mode;
    }
}

/// Consume a pending layered socket and drive its handshake in whichever
/// role `setUseClientMode` last left it in (default client). Returns the RAW
/// rustls stream id — the caller applies `RUSTLS_SOCK_ID_BASE`, matching
/// every other rustls stream id in this module.
pub fn drive_pending_layered_handshake(pending_id: i32) -> Result<i32, String> {
    let pending = pending_layered_sockets()
        .lock()
        .remove(&pending_id)
        .ok_or_else(|| "layered socket handshake state missing".to_string())?;
    if crate::nbflags().dbg_tls_pls {
        eprintln!(
            "[dbg-tls-pls] drive: pending_id={} client_mode={} stream={} host={}",
            pending_id,
            pending.client_mode,
            match &pending.stream {
                PendingLayeredStream::Reused(_) => "Reused",
                PendingLayeredStream::DialFresh { .. } => "DialFresh",
            },
            pending.host,
        );
    }
    // gc-common w14-b: the stream row the handshake registers belongs to the
    // VM that stashed the socket.
    let owner_vm = pending.vm;
    let result = drive_pending_layered_handshake_inner(pending);
    if let Ok(id) = &result {
        rustls_claim_for_vm(owner_vm, *id);
    }
    result
}

/// The handshake half of [`drive_pending_layered_handshake`], which claims the
/// row this registers for the stashing VM.
pub fn drive_pending_layered_handshake_inner(pending: PendingLayeredSocket) -> Result<i32, String> {
    if pending.client_mode {
        // Built here, not in `stash_pending_layered_socket`, so a
        // `setEnabledCipherSuites` call made any time between `createSocket()`
        // and the handshake actually starting is honored — see
        // `PendingLayeredSocket::enabled_ciphers`'s doc.
        let trust_roots = if pending.extra_roots.is_empty() {
            None
        } else {
            Some(TlsTrustRoots {
                root_ders: pending.extra_roots,
                revocation: None,
            })
        };
        let roots = root_store_for_trust_roots(trust_roots.as_ref());
        let (provider, versions) =
            provider_and_versions(&pending.enabled_ciphers, &pending.enabled_protocols);
        let client_config = build_client_config_ex_with_provider(
            roots,
            &["http/1.1"],
            ClientAuthMode::Fixed(
                pending
                    .client_identity
                    .as_ref()
                    .map(|(cert, key)| (cert.as_str(), key.as_str())),
            ),
            None,
            pending.use_java_trust_manager,
            provider,
            &versions,
            None,
            None,
            // A layered client `SSLSocket`: JSSE checks the host name only for an endpoint identification
            // algorithm, applied at `startHandshake` (see `build_socket_client_config_with_identity_ciphers_in_vm`).
            true,
        )
        .map_err(|e| format!("layered client config: {e}"))?;
        match pending.stream {
            PendingLayeredStream::Reused(tcp) => {
                rustls_client_handshake_over_stream(tcp, client_config, &pending.host)
            }
            PendingLayeredStream::DialFresh { host, port } => {
                rustls_client_connect(client_config, &host, port)
            }
        }
    } else {
        let tcp = match pending.stream {
            PendingLayeredStream::Reused(tcp) => tcp,
            PendingLayeredStream::DialFresh { .. } => {
                // There is no meaningful "dial fresh" for SERVER mode — a
                // server accepts an inbound connection, it does not open an
                // outbound one. `wrapped`'s stream genuinely could not be
                // extracted (see this struct's doc); fail closed rather than
                // silently connecting somewhere nobody asked for.
                return Err(
                    "layered SSLSocket: server-mode handshake requires the wrapped \
                     Socket's own connection, which could not be extracted"
                        .to_string(),
                );
            }
        };
        let (cert_pem, key_pem) = pending
            .server_identity
            .ok_or_else(|| "No TLS key/cert configured for layered SSLSocket".to_string())?;
        rustls_server_handshake_over_stream(tcp, &cert_pem, &key_pem)
    }
}

/// Re-ask the registry AFTER a blocking rustls call has returned, and report a
/// concurrent `close()` as a close rather than as EOF or as a peer error.
///
/// The twin of `servlet::s2_tls_classify_after_block`, and it exists for the
/// same reason: W7-53's close-aware loop parks in `poll` on a bounded slice and
/// ABANDONS the wait when the registry entry disappears, which a TLS record
/// layer cannot survive — a reader that returns between two of the `recv`s that
/// make up one record leaves the caller with a fragment and the stream
/// desynchronised. Classifying a call that has already returned has no such
/// hazard: at that instant the record layer is at rest, either with a whole
/// record delivered or with a failure of its own.
///
/// Note this is genuinely NOT the same as making the read close-aware. It
/// converts a wakeup into the RIGHT answer; something else still has to
/// produce the wakeup, and on Windows nothing can — see `rustls_stream_close`.
pub fn rustls_classify_after_block(id: i32, result: std::io::Result<usize>) -> std::io::Result<usize> {
    {
        let reg = sreg().lock();
        if reg.client_streams.contains_key(&id) || reg.server_streams.contains_key(&id) {
            return result;
        }
    }
    match result {
        // Bytes that arrived before the close are still delivered; the NEXT
        // call reports the close. Dropping them would lose data the peer
        // really sent, and a TLS record already fully decrypted into `buf` is
        // not something a close can retract.
        Ok(n) if n > 0 => Ok(n),
        _ => Err(std::io::Error::new(
            std::io::ErrorKind::Interrupted,
            "socket closed",
        )),
    }
}

pub fn rustls_stream_read(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let debug_srv = crate::nbflags().dbg_tls_srv;
    // LOCK DISCIPLINE (stw-takeover / accept-close-deadlock family): resolve
    // the id and clone the per-stream handle under `sreg()`, then RELEASE
    // `sreg()` before blocking. Holding the process-wide registry mutex
    // across a read that waits for a peer parks every other TLS operation in
    // the process behind it — including the peer's own write — and a thread
    // parked on a plain mutex inside a native call never reaches a safepoint,
    // so a concurrent STW waits for it forever. Identical reasoning to
    // `rustls_server_accept`'s fix; these sibling functions were missed then.
    let (client, server) = {
        let reg = sreg().lock();
        (
            reg.client_streams.get(&id).map(|e| e.stream.clone()),
            reg.server_streams.get(&id).map(|e| e.stream.clone()),
        )
    };
    if let Some(stream) = client {
        let mut e = stream.lock();
        // FIX (TestSsl.testSni[JSSE]): a plain `SSLSocket.getInputStream()
        // .read()` on the client side used to propagate rustls's raw
        // `UnexpectedEof` ("peer closed connection without sending TLS
        // close_notify") straight through as an `IOException`. Tomcat's own
        // server connector (also CratonVM/rustls) closes the raw socket
        // after writing a `Connection: Close` response without a clean TLS
        // shutdown — an unclean-but-benign close real JSSE clients
        // routinely tolerate at the end of a fully-framed HTTP response.
        // Reuse the same EOF-tolerant read already established for the
        // native HTTP client bridge (`http_url_connection::
        // read_eof_tolerant`) instead of duplicating the tolerance logic.
        let result = crate::http_url_connection::read_eof_tolerant(&mut *e, buf);
        drop(e);
        // W7-61: an unclean peer close and a close from ANOTHER THREAD OF THIS
        // VM both arrive here as `Ok(0)`. Only the registry can tell them
        // apart, and only after the call — see `rustls_classify_after_block`.
        return rustls_classify_after_block(id, result);
    }
    if let Some(stream) = server {
        let mut e = stream.lock();
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_read ENTER id={} requested_len={}",
                id,
                buf.len()
            );
        }
        // `EintrIo`: a server-side `SSLSocket.getInputStream().read()` parks
        // in `recv` on a socket that carries `SO_RCVTIMEO`, which Linux
        // excludes from `SA_RESTART`. CratonVM's own cross-thread JIT
        // root-scan `SIGUSR2` therefore reaches Java as
        // `IOException: Interrupted system call`. The client-side arm above
        // gets the same treatment inside `read_eof_tolerant`.
        let result = match &mut *e {
            TlsServerStream::Rustls(s) => EintrIo::new(s).read(buf),
            TlsServerStream::Native(s) => EintrIo::new(s).read(buf),
            #[cfg(unix)]
            TlsServerStream::LegacyDsa(s) => EintrIo::new(s).read(buf),
            // Where JSSE surfaces a rejected handshake — see the variant's
            // doc comment. The caller turns this into `SSLHandshakeException`
            // via `s2_tls_handshake_failure`.
            TlsServerStream::HandshakeFailed { reason, .. } => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                reason.clone(),
            )),
        };
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_read RETURN id={} result={:?}",
                id, result
            );
        }
        drop(e);
        return rustls_classify_after_block(id, result);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such rustls stream id",
    ))
}

/// Write to either a client- or server-side rustls stream.
pub fn rustls_stream_write(id: i32, data: &[u8]) -> std::io::Result<usize> {
    let debug_srv = crate::nbflags().dbg_tls_srv;
    // Same lock discipline as `rustls_stream_read` — see its doc comment.
    let (client, server) = {
        let reg = sreg().lock();
        (
            reg.client_streams.get(&id).map(|e| e.stream.clone()),
            reg.server_streams.get(&id).map(|e| e.stream.clone()),
        )
    };
    if let Some(stream) = client {
        let mut e = stream.lock();
        let result = EintrIo::new(&mut *e).write(data);
        drop(e);
        // W7-61 — see `rustls_classify_after_block`.
        return rustls_classify_after_block(id, result);
    }
    if let Some(stream) = server {
        let mut e = stream.lock();
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_write ENTER id={} len={}",
                id,
                data.len()
            );
        }
        let result = match &mut *e {
            TlsServerStream::Rustls(s) => EintrIo::new(s).write(data),
            TlsServerStream::Native(s) => EintrIo::new(s).write(data),
            #[cfg(unix)]
            TlsServerStream::LegacyDsa(s) => EintrIo::new(s).write(data),
            // See the read arm.
            TlsServerStream::HandshakeFailed { reason, .. } => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                reason.clone(),
            )),
        };
        if debug_srv {
            eprintln!(
                "[dbg-tls-srv] stream_write RETURN id={} result={:?}",
                id, result
            );
        }
        drop(e);
        return rustls_classify_after_block(id, result);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such rustls stream id",
    ))
}

/// Close either a client- or server-side rustls stream (idempotent).
/// Access the descriptor independently of a reader holding the TLS stream lock.
pub fn with_pending_layered_tcp<R>(id: i32, f: impl FnOnce(&TcpStream) -> R) -> Option<R> {
    let table = pending_layered_sockets().lock();
    match &table.get(&id)?.stream {
        PendingLayeredStream::Reused(tcp) => Some(f(tcp)),
        _ => None,
    }
}

pub fn record_socket_client_local_chain(tls_id: i32, chain: Vec<Vec<u8>>) {
    if tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE {
        return;
    }
    if let Some(entry) = sreg()
        .lock()
        .client_streams
        .get_mut(&(tls_id - crate::servlet::RUSTLS_SOCK_ID_BASE))
    {
        entry.local_cert_chain_der = chain;
    }
}
pub fn socket_client_local_chain(tls_id: i32) -> Option<Vec<Vec<u8>>> {
    if tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE {
        return None;
    }
    let reg = sreg().lock();
    reg.client_streams
        .get(&(tls_id - crate::servlet::RUSTLS_SOCK_ID_BASE))
        .map(|e| e.local_cert_chain_der.clone())
        .filter(|c| !c.is_empty())
}

pub fn with_raw_tls_stream<R>(id: i32, f: impl FnOnce(&TcpStream) -> R) -> Option<R> {
    let reg = sreg().lock();
    reg.client_streams
        .get(&id)
        .and_then(|e| e.raw.as_ref())
        .or_else(|| reg.server_streams.get(&id).and_then(|e| e.raw.as_ref()))
        .map(f)
}

pub fn rustls_stream_close(id: i32) {
    // Unregister under `sreg()`, then do the graceful shutdown outside it.
    let (client, server) = {
        let mut reg = sreg().lock();
        let pair = (
            reg.client_streams.remove(&id),
            reg.server_streams.remove(&id),
        );
        // gc-common w14-b: only with a stream. A repeated close of a stale id
        // must not unown a listener that has since been handed that id.
        if pair.0.is_some() || pair.1.is_some() {
            reg.owners.remove(&id);
        }
        pair
    };
    // Send the alert while TCP is writable. Never wait for a stream mutex:
    // a reader may hold it while blocked in recv. The raw shutdown below still
    // wakes that reader when graceful shutdown cannot acquire the mutex.
    for raw in [
        client.as_ref().and_then(|e| e.raw.as_ref()),
        server.as_ref().and_then(|e| e.raw.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        let _ = raw.set_write_timeout(Some(std::time::Duration::from_millis(200)));
        let _ = raw.shutdown(std::net::Shutdown::Read);
    }
    if let Some(e) = client.as_ref() {
        if let Some(mut s) = e.stream.try_lock_for(std::time::Duration::from_millis(200)) {
            s.conn.send_close_notify();
            let _ = s.flush();
        }
    }
    if let Some(e) = server.as_ref() {
        if let Some(mut guard) = e.stream.try_lock_for(std::time::Duration::from_millis(200)) {
            match &mut *guard {
                TlsServerStream::Rustls(s) => {
                    s.conn.send_close_notify();
                    let _ = s.flush();
                }
                TlsServerStream::Native(s) => {
                    let _ = s.shutdown();
                }
                #[cfg(unix)]
                TlsServerStream::LegacyDsa(s) => {
                    let _ = s.shutdown();
                }
                // No session to close down gracefully; the `raw` shutdown
                // above has already ended the TCP connection.
                TlsServerStream::HandshakeFailed { .. } => {}
            }
        }
    }
    for raw in [
        client.as_ref().and_then(|e| e.raw.as_ref()),
        server.as_ref().and_then(|e| e.raw.as_ref()),
    ]
    .into_iter()
    .flatten()
    {
        let _ = raw.shutdown(std::net::Shutdown::Both);
    }
}

/// Why this accepted server-side stream's TLS handshake failed, or `None` if
/// it did not — see [`TlsServerStream::HandshakeFailed`]. Lets the Java-facing
/// read/write natives raise `SSLHandshakeException` (what JSSE raises at the
/// first I/O on such a socket) rather than a bare `IOException`.
pub fn rustls_server_handshake_failure(id: i32) -> Option<String> {
    let stream = {
        let reg = sreg().lock();
        reg.server_streams.get(&id).map(|e| e.stream.clone())
    }?;
    // `try_lock`: this is only ever called to CLASSIFY an error the caller
    // already has, so it must never park behind another thread's blocking
    // read. A failed stream's own read/write arm returns without waiting, so
    // losing the race here needs a second thread on the same socket, and
    // costs only the exception type.
    let guard = stream.try_lock()?;
    match &*guard {
        TlsServerStream::HandshakeFailed { reason, .. } => Some(reason.clone()),
        _ => None,
    }
}

/// Close a listener by id (idempotent).
pub fn rustls_listener_close(id: i32) {
    let mut reg = sreg().lock();
    if let Some(entry) = reg.listeners.remove(&id) {
        reg.owners.remove(&id);
        // Wake a thread parked in `rustls_server_accept`'s non-blocking poll
        // loop for this listener — see that function's doc comment.
        entry.closed.store(true, Ordering::SeqCst);
    }
}

/// Retrieve negotiated session info for either stream flavor.
pub fn rustls_session_info(
    id: i32,
) -> Option<(String, String, Option<String>, Option<String>)> {
    let reg = sreg().lock();
    if let Some(e) = reg.client_streams.get(&id) {
        return Some((
            e.negotiated_protocol.clone(),
            e.negotiated_cipher.clone(),
            e.negotiated_alpn.clone(),
            None,
        ));
    }
    if let Some(e) = reg.server_streams.get(&id) {
        return Some((
            e.negotiated_protocol.clone(),
            e.negotiated_cipher.clone(),
            e.negotiated_alpn.clone(),
            e.sni_hostname.clone(),
        ));
    }
    None
}

/// The peer certificate chain rustls captured during this CLIENT stream's
/// handshake, as raw DER — for `SSLSession.getPeerCertificates()`. Without
/// this, a caller that completes a handshake through the deferred
/// `SSLSocketFactory.createSocket(Socket,...)` path (`ensure_layered_handshake_started`)
/// and then calls `getPeerCertificates()` sees an empty chain and gets
/// `SSLPeerUnverifiedException("peer not authenticated")` even on a
/// perfectly successful handshake — confirmed via
/// `HttpComponentsClientHttpRequestFactoryBuilderTests.connectWithSslBundle`
/// (Apache HttpComponents' `AbstractClientTlsStrategy.verifySession()` calls
/// this immediately after every successful TLS upgrade). `rustls::ClientConnection`
/// retains this for the connection object's lifetime, so it's still readable
/// here despite being queried after the handshake loop that produced it has
/// already returned.
pub fn rustls_server_peer_cert_chain_der(id: i32) -> Option<Vec<Vec<u8>>> {
    let stream = { sreg().lock().server_streams.get(&id)?.stream.clone() };
    let guard = stream.lock();
    match &*guard {
        TlsServerStream::Rustls(stream) => stream
            .conn
            .peer_certificates()
            .map(|certs| certs.iter().map(|c| c.as_ref().to_vec()).collect()),
        TlsServerStream::Native(stream) => stream
            .peer_certificate()
            .ok()
            .flatten()
            .and_then(|c| c.to_der().ok())
            .map(|c| vec![c]),
        #[cfg(unix)]
        TlsServerStream::LegacyDsa(stream) => {
            let mut chain = Vec::new();
            if let Some(cert) = stream.ssl().peer_certificate() {
                if let Ok(der) = cert.to_der() {
                    chain.push(der);
                }
            }
            if let Some(certs) = stream.ssl().peer_cert_chain() {
                for cert in certs {
                    if let Ok(der) = cert.to_der() {
                        if !chain.contains(&der) {
                            chain.push(der);
                        }
                    }
                }
            }
            Some(chain)
        }
        TlsServerStream::HandshakeFailed { .. } => None,
    }
    .filter(|chain| !chain.is_empty())
}

pub fn rustls_client_peer_cert_chain_der(id: i32) -> Option<Vec<Vec<u8>>> {
    // Clone the per-stream handle under `sreg()` and release it before
    // touching the stream — see `rustls_stream_read`'s lock-discipline note.
    let stream = {
        let reg = sreg().lock();
        reg.client_streams.get(&id)?.stream.clone()
    };
    let entry = stream.lock();
    let certs = entry.conn.peer_certificates()?;
    if certs.is_empty() {
        return None;
    }
    Some(certs.iter().map(|c| c.as_ref().to_vec()).collect())
}

// -----------------------------------------------------------------------------
// Native method registrations
// -----------------------------------------------------------------------------

/// Width handed to `try_alloc_concurrent_synthetic` for an `SSLServerSocket`.
///
/// It is the SYNTHETIC width and nothing else. Under `--real-jdk` — and in
/// every default real-JDK build — `javax.net.ssl.SSLServerSocket` is a real
/// loaded class and the allocator gives the object the REAL layout, so this
/// number governs only the fabricated-class build.
///
/// **G25 — there are no `SSS_LISTENER_ID` / `SSS_LOCAL_PORT` / `SSS_CLOSED`
/// slot constants any more, and there must never be again.** MEASURED,
/// `javap -p java.net.ServerSocket` on JDK 25.0.3+9 (`SSLServerSocket` itself
/// declares no instance fields, so these ARE the object's slots):
///
/// ```text
///   0 private final    java.net.SocketImpl            impl
///   1 private volatile boolean                        created
///   2 private volatile boolean                        bound
///   3 private volatile boolean                        closed
///   4 private final    java.lang.Object               socketLock
///   5 private volatile java.util.Set<SocketOption<?>> options
/// ```
///
/// This file used to write its listener id into slot 0, its local port into
/// slot 1 and its closed flag into slot 2. Row by row (record
/// `G25-1-the-int-written-into-a-reference-slot-20260817.md`, and the sweep in
/// `G16-1-...`):
///
/// * `Int(listener_id)` -> `impl`, a REFERENCE field. The field-layout guard
///   does not let a non-reference land in a reference slot (it warns W7-84 and
///   boxes it), so `impl` never became the listener id and never became a
///   `SocketImpl` either — it stayed null, and every inherited
///   `java.net.ServerSocket` method whose bytecode calls `getImpl()` threw
///   `NullPointerException`. That is where `RSslLiveSession` died on its FIRST
///   statement, `ss.setSoTimeout(20000)`.
/// * `Int(local_port)` -> `created`, so a listener on any non-zero port
///   reported `created = true`.
/// * `Int(closed)` -> `bound`, so `close()` made the socket become BOUND:
///   `isBound()` read `false` while open and `true` after close, the exact
///   inversion visible in the sweep.
/// * `Object(None)` -> `closed`, a reference into a boolean slot.
///
/// [`SslServerSocketState`] was already the authority for all three values —
/// every reader consulted it first and only fell back to the field — so the
/// writes bought nothing and cost the whole inherited surface.
pub const SSS_FIELDS: usize = 4;

/// The authoritative, out-of-object state of one `javax.net.ssl.SSLServerSocket`.
///
/// `SSLServerSocket` is a real JDK class, so its loaded instance layout is not
/// the compact synthetic layout the TLS listener bridge wants (see
/// [`SSS_FIELDS`] for the measured layout and what writing into it did). This
/// table is now the ONLY record: there is no field fallback left to read,
/// because every slot a fallback could read belongs to `java.net.ServerSocket`
/// and means something else.
///
/// Keyed by [`gc_stable_objref_key`].
#[derive(Clone)]
pub struct SslServerSocketState {
    pub listener_id: i32,
    pub local_port: i32,
    pub closed: i32,
    /// Literal host address this listener is bound to — `"0.0.0.0"` for the
    /// two wildcard `createServerSocket` overloads, otherwise the
    /// `InetAddress.getHostAddress()` of the address the caller passed.
    /// Answers `getInetAddress()` / `getLocalSocketAddress()`.
    pub bind_address: String,
    /// `1` once a listener exists for this socket, `0` for one that
    /// `SSLServerSocketFactory.createServerSocket()` (the NO-ARG overload)
    /// made and nobody has bound. `isBound()` cannot be "we have a record of
    /// it" any more, because there is now a construction path that records an
    /// UNBOUND socket.
    pub bound: i32,
    /// `InetAddress.toString()` of the address actually bound, captured at
    /// creation time from the caller's own object, because that rendering
    /// (`hostname/literal`, hostname omitted when the address was built from a
    /// literal) is not reconstructible from the literal alone. Answers
    /// `toString()`.
    pub bind_display: String,
}

/// A miss in [`ssl_server_socket_states`] — an `SSLServerSocket` this module
/// did not create, which under `--real-jdk` cannot happen, because all three
/// `SSLServerSocketFactory.createServerSocket` overloads are intercepted.
///
/// The answers are deliberately the pessimistic ones the field fallbacks used
/// to produce for a totally unknown object (`closed = 1`, no listener), NOT
/// the ones a real unbound `ServerSocket` gives. A socket this file has no
/// record of is one it cannot accept on, and saying so is the honest answer.
pub fn ssl_server_socket_state_miss() -> SslServerSocketState {
    SslServerSocketState {
        listener_id: -1,
        local_port: 0,
        closed: 1,
        bound: 0,
        bind_address: String::new(),
        bind_display: String::new(),
    }
}

pub fn ssl_server_socket_states() -> &'static Mutex<HashMap<u64, SslServerSocketState>> {
    static STATES: OnceLock<Mutex<HashMap<u64, SslServerSocketState>>> = OnceLock::new();
    STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn ssl_server_socket_state(
    ctx: &dyn NativeContext,
    socket: ObjectRef,
) -> Option<SslServerSocketState> {
    // Key before the guard (gc-common w26-b): it takes the lock-key registry.
    let key = existing_objref_key(ctx, socket)?;
    ssl_server_socket_states().lock().get(&key).cloned()
}

pub fn set_ssl_server_socket_state(
    ctx: &dyn NativeContext,
    socket: ObjectRef,
    state: SslServerSocketState,
) {
    let key = gc_stable_objref_key(ctx, socket);
    ssl_server_socket_states().lock().insert(key, state);
}

/// `listener_id` → the server identity its rustls `ServerConfig` was built
/// from.
///
/// STUB-REMOVAL (wave 2): `SSLServerSocket.setNeedClientAuth`/
/// `setWantClientAuth` were unconditional no-ops. A server that stood up an
/// mTLS listener therefore accepted every anonymous client while believing a
/// client certificate was mandatory — a silently disabled authentication
/// check, the most dangerous shape a constant native can take. Honouring the
/// call means rebuilding the listener's `ServerConfig` with a
/// `WebPkiClientVerifier` (`build_server_config_single_cert_ex`), which needs
/// the (cert, key) PEM the listener was originally built from — the
/// `TlsServerListenerEntry` only keeps the finished config, so record the
/// ingredients here at `createServerSocket` time.
pub fn sss_listener_identities() -> &'static Mutex<HashMap<i32, RuntimeTlsIdentity>> {
    static T: OnceLock<Mutex<HashMap<i32, RuntimeTlsIdentity>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-`SSLServerSocket` client-auth request as last set by
/// `setNeedClientAuth`/`setWantClientAuth`, keyed by `gc_stable_objref_key`.
/// Tuple is `(need, want)`, each 0/1; JSSE makes the two mutually exclusive.
pub fn sss_client_auth_states() -> &'static Mutex<HashMap<u64, (i32, i32)>> {
    static T: OnceLock<Mutex<HashMap<u64, (i32, i32)>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Non-destructive client-CA source for the rebuild above. Deliberately NOT
/// `active_client_trust_roots()`: that one *takes* the thread-local selected
/// context slot, which a later client handshake on the same thread still
/// needs.
pub fn sss_client_ca_pem(vm: usize) -> String {
    let roots = selected_context_trust_roots().or_else(|| huc_default_trust_roots_in_vm(vm));
    trust_roots_pem(roots.as_ref())
}

/// Apply a `setNeedClientAuth`/`setWantClientAuth` request to the live
/// listener by rebuilding its `ServerConfig`.
///
/// Fails LOUDLY rather than silently when the request cannot be honoured: a
/// caller that asked for mandatory client authentication and got no exception
/// is entitled to assume it is in force. Turning client auth OFF is the state
/// the listener was already built in, so that direction never throws.
pub fn sss_apply_client_auth(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    need: bool,
    want: bool,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    let key = gc_stable_objref_key(ctx, this);
    if !need && !want {
        sss_client_auth_states().lock().insert(key, (0, 0));
        return Ok(None);
    }
    let state = ssl_server_socket_state(ctx, this);
    // An UNBOUND socket — the no-arg `SSLServerSocketFactory
    // .createServerSocket()` overload — has no listener to rebuild yet, so
    // recording the flag is the whole of the work and NOT the
    // silently-ignored setter this function exists to prevent: `bind` refuses
    // such a socket outright (see its registration), so no listener can ever
    // come up without the verifier the caller asked for.
    if state
        .as_ref()
        .is_some_and(|state| state.bound == 0 && state.closed == 0)
    {
        sss_client_auth_states()
            .lock()
            .insert(key, (i32::from(need), i32::from(want)));
        return Ok(None);
    }
    let listener_id = state.map(|state| state.listener_id).unwrap_or(-1);
    if listener_id < 0 {
        return Err(RuntimeError::IOException {
            message: "SSLServerSocket is closed".into(),
        }
        .into());
    }
    let identity = sss_listener_identities()
        .lock()
        .get(&listener_id)
        .cloned()
        .ok_or_else(|| RuntimeError::IllegalStateException {
            message: "client auth requested on a listener with no recorded TLS identity"
                .to_string(),
        })?;
    let client_ca = match identity.client_ca_pem.as_deref() {
        Some(ca) => ca.to_string(),
        None => {
            let pem = sss_client_ca_pem(ctx.vm_identity());
            if pem.is_empty() {
                return Err(RuntimeError::IllegalStateException {
                    message: "setNeedClientAuth(true) requires javax.net.ssl.trustStore"
                        .to_string(),
                }
                .into());
            }
            pem
        }
    };
    let config = build_server_config_single_cert_ex(
        &identity.cert_pem,
        &identity.key_pem,
        &["h2", "http/1.1"],
        need,
        want,
        Some(client_ca.as_str()),
    )
    .map_err(|message| RuntimeError::IOException { message })?;
    {
        let mut reg = sreg().lock();
        match reg.listeners.get_mut(&listener_id) {
            Some(entry) => entry.config = TlsServerConfig::Rustls(config),
            None => {
                return Err(RuntimeError::IOException {
                    message: "SSLServerSocket is closed".into(),
                }
                .into());
            }
        }
    }
    sss_client_auth_states()
        .lock()
        .insert(key, (i32::from(need), i32::from(want)));
    Ok(None)
}

// Server-side SSLSocket returned from accept(): reuses the existing
// SSLSocket 6-field layout but field 2 (tls_id) references the rustls
// server-streams table rather than the native-tls client table. The
// read/write streams distinguish by class name.
pub const SSS_SOCK_HOST: usize = 0;
pub const SSS_SOCK_PORT: usize = 1;
pub const SSS_SOCK_TLSID: usize = 2;
pub const SSS_SOCK_CLOSED: usize = 3;
pub const SSS_SOCK_SESSION: usize = 4;
pub const SSS_SOCK_FIELDS: usize = 6;

/// Register T2.7's server-side and HTTPS natives. Called from
/// `register_phase68_natives` after `register_p68_ssl`.
pub fn register_t27_natives(r: &mut NativeMethodRegistry) {
            register_sslserversocket(r);
    register_accepted_issuers(r);
    register_https_url_connection(r);
    register_self_test(r);
    register_alpn_accessor(r);
    register_client_socket_mode_accessors(r);
    // E31: must run after `register_p68_ssl` (lib.rs calls this function at
    // ~18540, that one at 18474) — but nothing else registers this triple in
    // either mode, so the ordering is a property to preserve rather than a
    // conflict to win. See the function's own doc for the oracle.
    register_socket_handshake_session(r);
    }

pub fn register_accepted_issuers(r: &mut NativeMethodRegistry) {
            // T2.7.4: real X509TrustManager.getAcceptedIssuers — returns a Java
    // `java.security.cert.X509Certificate[]` whose element at index i carries
    // the DER-encoded subject DN of the i-th trusted root. Uses
    // rustls-native-certs to enumerate the host trust store.
    r.register(
        "javax/net/ssl/X509TrustManager",
        "getAcceptedIssuers",
        "()[Ljava/security/cert/X509Certificate;",
        |ctx, _args| {
            let ders = accepted_issuer_ders();
            // `X509Certificate[]`, the declared type (i6-L2).
            let component = crate::lang_class::reflection_component_id(
                ctx,
                "java/security/cert/X509Certificate",
            );
            let arr0 = ctx.new_ref_array(component, ders.len());
            // GC: everything in this loop allocates — the mirror, two strings,
            // the DER byte[] — so `arr` and `cert` must be pinned and re-read,
            // not held raw. See `x509_manager::get_accepted_issuers`, which
            // had the identical defect and the netty failure that found it.
            let pin = ctx.pin_native_root(arr0);
            let mut arr = arr0;
            let result = (|| -> Result<(), cratonvm_types::error::MethodCallFailed> {
                for (i, der) in ders.iter().enumerate() {
                    let cert0 = try_alloc_concurrent_synthetic(
                        ctx,
                        "java/security/cert/X509Certificate",
                        4,
                    )?;
                    let cert_pin = ctx.pin_native_root(cert0);
                    // Best-effort CN extraction via the existing DER parser.
                    let (subject, issuer) = crate::phases_late::basic_der_extract_names(der)
                        .unwrap_or_else(|| ("CN=Unknown".into(), "CN=Unknown".into()));
                    let sub = ctx.create_string(&subject);
                    let iss = ctx.create_string(&issuer);
                    let cert = ctx.read_native_pin(cert_pin, cert0);
                    ctx.set_field(cert, 0, Value::Object(Some(sub)));
                    ctx.set_field(cert, 1, Value::Object(Some(iss)));
                    ctx.set_field(cert, 2, Value::Long(0));
                    let der_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, der.len());
                    for (j, &b) in der.iter().enumerate() {
                        ctx.set_array_element(der_arr, j, Value::Int(b as i8 as i32));
                    }
                    let cert = ctx.read_native_pin(cert_pin, cert0);
                    ctx.set_field(cert, 3, Value::Object(Some(der_arr)));
                    ctx.unpin_native_roots(cert_pin);
                    arr = ctx.read_native_pin(pin, arr0);
                    ctx.set_array_element(arr, i, Value::Object(Some(cert)));
                }
                Ok(())
            })();
            ctx.unpin_native_roots(pin);
            result?;
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// OpenSSL-backed fallback acceptor for identities rustls's `ring` crypto
/// backend refuses to sign with. Originally written for DSA (rustls has no
/// DSA `SigningKey` at all), but `ring::rsa::KeyPair::from_pkcs8` *also*
/// rejects any RSA key below 2047 bits (a hard-coded policy floor, not a
/// parsing failure) — `rustls::sign::any_supported_type` surfaces that as the
/// same generic "failed to parse private key as RSA, ECDSA, or EdDSA" rustls
/// gives for a genuinely-unparseable key, with no way to distinguish the two
/// from the caller side. H2's bundled `TestNetUtils` self-signed test
/// keystore carries exactly this: a legacy 1024-bit RSA key (generated 2005)
/// that's syntactically well-formed PKCS#8 RSA but below ring's floor. Since
/// OpenSSL enforces no such minimum (once `set_security_level(0)` is applied
/// below), it accepts whatever key/cert pair rustls's stricter backend
/// wouldn't — DSA, sub-2047-bit RSA, or any other legacy identity — so this
/// accepts any key OpenSSL itself can use rather than gating on key type.
/// Build a real TLS listener and its Java `SSLServerSocket` wrapper.  Keep
/// every `SSLServerSocketFactory.createServerSocket` overload on this one
/// path so callers cannot accidentally fall through to `ServerSocketFactory`'s
/// plaintext implementation.
#[cfg(unix)]
pub fn legacy_dsa_acceptor(cert_pem: &str, key_pem: &str) -> Result<SslAcceptor, String> {
    let key = PKey::private_key_from_pem(key_pem.as_bytes()).map_err(|e| e.to_string())?;
    let cert = X509::from_pem(cert_pem.as_bytes()).map_err(|e| e.to_string())?;
    let mut builder =
        SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server()).map_err(|e| e.to_string())?;
    builder.set_security_level(0);
    builder
        .set_cipher_list("ALL:@SECLEVEL=0")
        .map_err(|e| e.to_string())?;
    builder.set_private_key(&key).map_err(|e| e.to_string())?;
    builder.set_certificate(&cert).map_err(|e| e.to_string())?;
    builder.check_private_key().map_err(|e| e.to_string())?;
    Ok(builder.build())
}

/// Name the algorithm of a PKCS#8 private key by its `AlgorithmIdentifier` OID.
///
/// Only used to explain a refusal. rustls reports every unusable key with the
/// same "failed to parse private key as RSA, ECDSA, or EdDSA" no matter why, so
/// a reader of that message cannot tell a corrupt key from a well-formed one of
/// a type no backend here supports. Naming the algorithm is the difference
/// between "the keystore is broken" and "this identity needs a TLS backend we
/// do not have on this platform".
pub fn pkcs8_algorithm_name(der: &[u8]) -> Option<&'static str> {
    // SEQUENCE { INTEGER version, SEQUENCE { OID algorithm, ... }, ... }
    fn tlv(buf: &[u8], at: usize) -> Option<(u8, usize, usize)> {
        let tag = *buf.get(at)?;
        let len_byte = *buf.get(at + 1)?;
        let mut p = at + 2;
        let len = if len_byte & 0x80 != 0 {
            let n = (len_byte & 0x7f) as usize;
            if n > 4 || p + n > buf.len() {
                return None;
            }
            let mut len = 0usize;
            for byte in &buf[p..p + n] {
                len = len.checked_mul(256)?.checked_add(*byte as usize)?;
            }
            p += n;
            len
        } else {
            len_byte as usize
        };
        // The declared end may lie past the buffer — callers here read only the
        // header, and a truncated key still names its algorithm. Every actual
        // byte read below goes through `get`, so an over-long length cannot
        // reach past the slice.
        Some((tag, p, p.checked_add(len)?))
    }
    let (tag, outer, _) = tlv(der, 0)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, _, after_version) = tlv(der, outer)?;
    if tag != 0x02 {
        return None;
    }
    let (tag, alg_body, _) = tlv(der, after_version)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, oid_start, oid_end) = tlv(der, alg_body)?;
    if tag != 0x06 {
        return None;
    }
    match der.get(oid_start..oid_end)? {
        // 1.2.840.113549.1.1.1 rsaEncryption
        [0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01] => Some("RSA"),
        // 1.2.840.10040.4.1 id-dsa
        [0x2a, 0x86, 0x48, 0xce, 0x38, 0x04, 0x01] => Some("DSA"),
        // 1.2.840.10045.2.1 id-ecPublicKey
        [0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01] => Some("EC"),
        // 1.3.101.112 / 1.3.101.113 Ed25519 / Ed448
        [0x2b, 0x65, 0x70] => Some("Ed25519"),
        [0x2b, 0x65, 0x71] => Some("Ed448"),
        _ => None,
    }
}

/// Explain, in the exception text, why a server identity rustls refused has no
/// second chance on this platform.
///
/// On Unix `legacy_dsa_acceptor` (OpenSSL) picks these up. On Windows there is
/// no equivalent and there cannot be a platform one: TLS with a DSA certificate
/// requires the `TLS_DHE_DSS_*` cipher suites, rustls implements no DHE at all,
/// and Windows SChannel has offered zero DSS suites since Windows 10 (measured
/// on Windows 11: `Get-TlsCipherSuite` lists 28 suites, none DSS). So
/// `native_tls`'s failure there is not an import bug to be fixed by feeding it
/// a PKCS#12 instead — the handshake could not be negotiated afterwards either.
#[cfg(not(unix))]
pub fn legacy_identity_hint(key_pem: &str) -> String {
    let algorithm = parse_private_key_pem(key_pem)
        .ok()
        .and_then(|key| pkcs8_algorithm_name(key.secret_der()))
        .unwrap_or("unrecognised");
    if algorithm == "DSA" {
        " -- the server identity carries a DSA key; TLS with a DSA certificate needs the \
         TLS_DHE_DSS_* cipher suites, which neither rustls nor Windows SChannel provides. \
         CratonVM's OpenSSL-backed legacy fallback is Unix-only (see \
         native-builtins/src/t27_tls.rs, legacy_dsa_acceptor)"
            .to_string()
    } else {
        format!(" -- server identity key algorithm: {algorithm}")
    }
}

/// `InetAddress.toString()` for the two `createServerSocket` overloads that
/// take no address. MEASURED on HotSpot 25.0.3+9 (`G25Probe`,
/// `ssl.wild.getInetAddress`): a `ServerSocket` bound to the wildcard reports
/// `0.0.0.0/0.0.0.0`, not `/0.0.0.0` — its address is
/// `InetAddress.anyLocalAddress()`, whose cached host NAME is the literal
/// `"0.0.0.0"`, which `InetAddress.getByName("0.0.0.0")` does NOT reproduce.
pub const SSS_WILDCARD_BIND: &str = "0.0.0.0";
pub const SSS_WILDCARD_DISPLAY: &str = "0.0.0.0/0.0.0.0";

pub fn create_ssl_server_socket(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    port: i32,
    bind_address: &str,
    bind_display: &str,
) -> Result<Option<Value>, cratonvm_types::error::MethodCallFailed> {
    if !(0..=65535).contains(&port) {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("port out of range: {port}"),
        }
        .into());
    }
    // SSLContext.getServerSocketFactory() retains its context in field zero.
    // Prefer that per-context identity: Spring SSL bundles commonly build
    // multiple contexts in one process, so the process-wide keystore slot may
    // have been replaced by an unrelated client context by the time LDAPS
    // starts its listener. getDefault() returns an unbound factory and keeps
    // the established runtime-identity fallback for that case.
    let configured = match args.first() {
        Some(Value::Object(Some(factory))) if ctx.object_num_fields(*factory) > 0 => {
            match ctx.get_field(*factory, 0) {
                Value::Object(Some(ssl_context)) => ctx_identity(ctx, ssl_context)?,
                _ => None,
            }
        }
        _ => None,
    };
    let fallback = require_runtime_tls_identity(ctx.vm_identity())?;
    let identity = configured
        .map(|(cert_pem, key_pem)| RuntimeTlsIdentity {
            cert_pem,
            key_pem,
            client_ca_pem: None,
        })
        .unwrap_or(fallback);
    let config = build_server_config_single_cert(
        &identity.cert_pem,
        &identity.key_pem,
        &["h2", "http/1.1"],
        false,
        None,
    )
    .map(TlsServerConfig::Rustls)
    .or_else(|rustls_error| {
        #[cfg(unix)]
        {
            legacy_dsa_acceptor(&identity.cert_pem, &identity.key_pem)
                .map(TlsServerConfig::LegacyDsa)
                .map_err(|legacy_error| {
                    format!("{rustls_error}; legacy DSA TLS fallback: {legacy_error}")
                })
        }
        #[cfg(not(unix))]
        {
            native_tls::Identity::from_pkcs8(
                identity.cert_pem.as_bytes(),
                identity.key_pem.as_bytes(),
            )
            .and_then(native_tls::TlsAcceptor::new)
            .map(TlsServerConfig::Native)
            .map_err(|native_error| {
                format!(
                    "{rustls_error}; platform TLS fallback: {native_error}{}",
                    legacy_identity_hint(&identity.key_pem)
                )
            })
        }
    })
    .map_err(|message| RuntimeError::IOException { message })?;

    let listener = TcpListener::bind((bind_address, port as u16)).map_err(|error| {
        RuntimeError::IOException {
            message: format!("bind {bind_address}:{port}: {error}"),
        }
    })?;
    let local_port = listener
        .local_addr()
        .map(|address| address.port())
        .unwrap_or(port as u16);

    let entry = TlsServerListenerEntry {
        listener,
        config,
        local_port,
        closed: Arc::new(AtomicBool::new(false)),
    };
    // gc-common w14-b: owned by this VM, so its teardown closes the listener
    // and frees the port (`forget_vm_rustls_sockets`).
    let vm = ctx.vm_identity();
    let id = {
        let mut reg = sreg().lock();
        let id = alloc_server_id(&mut reg);
        reg.listeners.insert(id, entry);
        reg.owners.insert(id, vm);
        id
    };
    // Remember the ingredients so `setNeedClientAuth`/`setWantClientAuth` can
    // rebuild this listener's config with a client verifier — see
    // `sss_listener_identities`.
    sss_listener_identities().lock().insert(id, identity);

    let obj = crate::tls_carriers::alloc_tls_carrier(
        ctx,
        crate::tls_carriers::SSL_SERVER_SOCKET_PUBLIC,
        crate::tls_carriers::SSL_SERVER_SOCKET_IMPL,
        SSS_FIELDS,
    )?;
    // G25: the side table is the WHOLE record. The four `ctx.set_field` calls
    // that used to stand here wrote into `java.net.ServerSocket`'s real
    // `impl`/`created`/`bound`/`closed` — see [`SSS_FIELDS`] for the measured
    // layout, what each write actually did, and why the first of them is the
    // reason `getImpl()` was null on every inherited method.
    set_ssl_server_socket_state(
        ctx,
        obj,
        SslServerSocketState {
            listener_id: id,
            local_port: local_port as i32,
            closed: 0,
            bound: 1,
            bind_address: bind_address.to_string(),
            bind_display: bind_display.to_string(),
        },
    );
    Ok(Some(Value::Object(Some(obj))))
}

/// The exact `SSLServerSocket.toString()` for a socket in `state`.
///
/// Split out from the registration so the rendering is checkable without a VM
/// — `sss_to_string_matches_the_oracle` pins both shapes. MEASURED on HotSpot
/// 25.0.3+9 (`G25Probe`); note the `[SSL: ...]` wrapper, which
/// `sun.security.ssl.SSLServerSocketImpl` adds by OVERRIDING
/// `ServerSocket.toString()`.
pub fn sss_to_string(state: Option<&SslServerSocketState>) -> String {
    match state {
        Some(state) if state.bound != 0 => format!(
            "[SSL: ServerSocket[addr={},localport={}]]",
            state.bind_display, state.local_port
        ),
        // A socket this file made but never bound (`createServerSocket()`): HotSpot's Impl wraps the
        // inherited rendering too, `[SSL: ServerSocket[unbound]]` (Nio21SocketImpl).
        Some(_) => "[SSL: ServerSocket[unbound]]".to_string(),
        // The exact constant the inherited `java.net.ServerSocket.toString()`
        // bytecode produces while `isBound()` is false, so a socket this file
        // has no record of reads the same either way.
        None => "ServerSocket[unbound]".to_string(),
    }
}

/// The state `close()` leaves behind.
///
/// MEASURED (`G25Probe`): closing a `ServerSocket` does NOT unbind it — the
/// bind identity, the local port and the recorded rendering all survive, and
/// only `isClosed()` moves. Carrying `..state` forward is therefore the
/// contract, not an optimisation; clearing the bind fields here would make
/// `getInetAddress()` on a closed socket answer `null` where the oracle
/// answers `/127.0.0.1`.
pub fn sss_closed_state(state: SslServerSocketState) -> SslServerSocketState {
    SslServerSocketState {
        listener_id: -1,
        closed: 1,
        ..state
    }
}

/// The bind address of a `createServerSocket(int, int, InetAddress)` call, as
/// the pair [`SslServerSocketState`] records: the literal
/// `getHostAddress()` (what `TcpListener::bind` needs, and what
/// `getInetAddress()` is rebuilt from) and the caller's own
/// `InetAddress.toString()` (what `toString()` prints).
///
/// **Both, not one.** `InetAddress.toString()` is `hostName + "/" +
/// getHostAddress()` reading the CACHED name field, so an address the caller
/// built from a hostname renders `localhost/127.0.0.1` while the same literal
/// put back through `InetAddress.getByName` renders `/127.0.0.1`. The name
/// cannot be recovered from the literal, and asking for it later would mean a
/// reverse DNS lookup inside a `toString()`. Capture it once, here, where the
/// caller's object is in hand.
pub fn ssl_server_bind_address(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    index: usize,
) -> Result<(String, String), cratonvm_types::error::MethodCallFailed> {
    let address = obj_arg(args, index)?;
    let pin_base = ctx.pin_native_root(address);
    let resolved = ctx.invoke_virtual(address, "getHostAddress", "()Ljava/lang/String;", &[]);
    let host = match resolved {
        Ok(Some(Value::Object(Some(value)))) => ctx.read_string(value).unwrap_or_default(),
        Ok(_) => String::new(),
        Err(error) => {
            ctx.unpin_native_roots(pin_base);
            return Err(error);
        }
    };
    let address = ctx.read_native_pin(pin_base, address);
    let rendered = ctx.invoke_virtual(address, "toString", "()Ljava/lang/String;", &[]);
    ctx.unpin_native_roots(pin_base);
    let display = match rendered {
        Ok(Some(Value::Object(Some(value)))) => ctx.read_string(value).unwrap_or_default(),
        _ => String::new(),
    };
    if host.is_empty() {
        return Err(RuntimeError::IllegalArgumentException {
            message: "InetAddress has no host address".into(),
        }
        .into());
    }
    // A `toString()` that could not be read is not a reason to refuse the
    // bind; fall back to the rendering `InetAddress` gives an address with no
    // cached host name, which is what a literal produces anyway.
    let display = if display.is_empty() {
        format!("/{host}")
    } else {
        display
    };
    Ok((host, display))
}

pub fn register_sslserversocket(r: &mut NativeMethodRegistry) {
    // T2.7.9: SSLServerSocketFactory.createServerSocket(int port) —
    // binds a TcpListener on 0.0.0.0:port, builds a rustls ServerConfig
    // from the configured runtime TLS identity, and registers both in the
    // module registry.
    //
    // SECURITY: this entry point no longer falls back to any embedded
    // private key. Until `set_runtime_tls_identity(Some(_))` has been
    // called (typically from the `javax.net.ssl.keyStore` /
    // `SSLContext.init` plumbing), every invocation throws
    // `IllegalStateException("No TLS key/cert configured; set
    // javax.net.ssl.keyStore")` rather than silently serving traffic
    // under a publicly known key. Real KMF integration is plumbed through
    // the `SSLContext.getServerSocketFactory` code path in a follow-up
    // session once KMF.init actually stores a PKCS#12 identity into this
    // slot.
    let sssf = "javax/net/ssl/SSLServerSocketFactory";
    r.register(
        sssf,
        "createServerSocket",
        "(I)Ljava/net/ServerSocket;",
        |ctx, args| {
            let port = args.get(1).and_then(|v| v.as_int()).unwrap_or(0);
            create_ssl_server_socket(ctx, args, port, SSS_WILDCARD_BIND, SSS_WILDCARD_DISPLAY)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // UnboundID's LDAP listener calls these overloads (with backlog 128).
    // Without explicit bridges here, dispatch reaches `ServerSocketFactory`'s
    // plaintext implementation and an LDAPS client gets "wrong version
    // number" after the SocketFactory client-side fix succeeds.
    r.register(
        sssf,
        "createServerSocket",
        "(II)Ljava/net/ServerSocket;",
        |ctx, args| {
            let port = args.get(1).and_then(|value| value.as_int()).unwrap_or(0);
            create_ssl_server_socket(ctx, args, port, SSS_WILDCARD_BIND, SSS_WILDCARD_DISPLAY)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sssf,
        "createServerSocket",
        "(IILjava/net/InetAddress;)Ljava/net/ServerSocket;",
        |ctx, args| {
            let port = args.get(1).and_then(|value| value.as_int()).unwrap_or(0);
            let (bind_address, bind_display) = ssl_server_bind_address(ctx, args, 3)?;
            create_ssl_server_socket(ctx, args, port, &bind_address, &bind_display)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // `createServerSocket()` — the NO-ARG overload, which this file did not
    // have.
    //
    // `javax.net.ssl.SSLServerSocketFactory` inherits it from
    // `javax.net.ServerSocketFactory`, and `phases_early` registers a native
    // THERE that answers `new java.net.ServerSocket()`. Dispatch asks the
    // registry about the receiver's class chain, so an
    // `SSLServerSocketFactory` receiver reached that one: every caller of
    // `((SSLServerSocketFactory) SSLServerSocketFactory.getDefault())
    // .createServerSocket()` got a PLAIN `java.net.ServerSocket` back.
    //
    // Two rows of `L6TlsParamSweep` (`SSLServerSocket surface`, `SSLServerSocket
    // params round-trip`) died on the cast HotSpot does not have to make:
    //
    // ```text
    //   ClassCastException: class java.net.ServerSocket cannot be cast to
    //   class javax.net.ssl.SSLServerSocket
    // ```
    //
    // and the caller who did NOT cast got the worse half — a plaintext
    // listener from a factory whose whole name says TLS.
    //
    // `SSLServerSocketFactoryImpl.createServerSocket()` is
    // `new SSLServerSocketImpl(context)`: an SSLServerSocket with no listener
    // behind it, whose parameters can be set and read before anything binds.
    // That is what this records — an UNBOUND socket, the first this file has
    // ever had, which is why `SslServerSocketState::bound` exists.
    r.register(
        sssf,
        "createServerSocket",
        "()Ljava/net/ServerSocket;",
        |ctx, _args| {
            let obj = crate::tls_carriers::alloc_tls_carrier(
                ctx,
                crate::tls_carriers::SSL_SERVER_SOCKET_PUBLIC,
                crate::tls_carriers::SSL_SERVER_SOCKET_IMPL,
                SSS_FIELDS,
            )?;
            set_ssl_server_socket_state(
                ctx,
                obj,
                SslServerSocketState {
                    listener_id: -1,
                    // `getLocalPort()` on an unbound `ServerSocket` is -1, not
                    // 0 — the miss state's 0 is for a socket this file has no
                    // record of at all.
                    local_port: -1,
                    closed: 0,
                    bound: 0,
                    bind_address: String::new(),
                    bind_display: String::new(),
                },
            );
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sssf,
        "getDefault",
        "()Ljavax/net/ServerSocketFactory;",
        |ctx, _args| {
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
        sssf,
        "getDefaultCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Single source of truth — see `SUPPORTED_CIPHER_SUITE_NAMES`.
            // Three TLS 1.3 names stood here while `SSLSocketFactory` and
            // `SSLContext` answered all fifteen, so one VM gave two answers
            // to the same question depending on which factory was asked.
            let suites = SUPPORTED_CIPHER_SUITE_NAMES;
            let arr = materialize_java_string_array(ctx, suites);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // SSLServerSocket methods
    let sss = "javax/net/ssl/SSLServerSocket";
    r.register(sss, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.local_port)
                .unwrap_or(ssl_server_socket_state_miss().local_port),
        )))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.closed)
                .unwrap_or(ssl_server_socket_state_miss().closed),
        )))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = ssl_server_socket_state(ctx, this).unwrap_or_else(ssl_server_socket_state_miss);
        let id = state.listener_id;
        if id >= 0 {
            rustls_listener_close(id);
            // Drop the recorded identity with the listener it belongs to (see
            // `sss_listener_identities`), so the table cannot grow without
            // bound and a recycled listener id cannot inherit stale key
            // material.
            sss_listener_identities().lock().remove(&id);
        }
        // MEASURED (`G25Probe`, HotSpot 25.0.3+9): closing a `ServerSocket`
        // does NOT unbind it. `isBound()`, `getInetAddress()`,
        // `getLocalPort()`, `getLocalSocketAddress()` and `toString()` all keep
        // answering exactly what they answered while it was open — only
        // `isClosed()` moves. So the bind identity is carried forward here
        // rather than cleared.
        set_ssl_server_socket_state(ctx, this, sss_closed_state(state));
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let accepted_protocols = lookup_sss_enabled_protocols(ctx, this)
            .unwrap_or_else(|| vec!["TLSv1.3".into(), "TLSv1.2".into()]);
        let accepted_mode = sss_mode_state(ctx, this);

        let id = ssl_server_socket_state(ctx, this)
            .map(|state| state.listener_id)
            .unwrap_or(ssl_server_socket_state_miss().listener_id);
        if id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLServerSocket is closed".into(),
            }
            .into());
        }
        let accept_timeout = sss_accept_timeout(ctx, this);
        // `rustls_server_accept` parks this thread — unboundedly in its poll
        // loop, then up to the socket's 30s timeout in the handshake — and
        // marks itself GC-blocked across those waits via `gc_blocked_syscall`,
        // which reads this thread-local. Publishing `ctx` here is what makes
        // that guard live on the acceptor thread; without it the guard is
        // inert and the thread is again a mutator that can never cooperate.
        let stream_id = {
            let _active_ctx = set_active_native_context(ctx);
            rustls_server_accept_within(id, accept_timeout)
        };
        let stream_id = match stream_id {
            Ok(stream_id) => stream_id,
            Err(AcceptFailure::TimedOut) => {
                // The one place this file must NOT answer `IOException`:
                // `java.net.SocketTimeoutException` is what a caller catches to
                // mean "no peer yet, loop again" — `RSslLiveSession.serve` is
                // written exactly that way — and it is a SUBCLASS of
                // `InterruptedIOException`, so an `IOException` here is caught
                // by the same handlers and read as a dead listener.
                return Err(crate::phases_early::throw_jca_exc(
                    ctx,
                    "java/net/SocketTimeoutException",
                    "Accept timed out",
                ));
            }
            Err(AcceptFailure::Failed(message)) => {
                return Err(RuntimeError::IOException { message }.into())
            }
        };

        // The id space the STREAM natives key on is the offset one:
        // `s2_tls_read`/`s2_tls_write` route to the rustls tables only for ids
        // >= `RUSTLS_SOCK_ID_BASE` and otherwise look the id up in the
        // native-tls registry, where an accepted rustls stream does not exist.
        // `rustls_session_info` is keyed by the RAW rid, so that one call below
        // deliberately keeps `stream_id`. Same convention as
        // `ensure_layered_handshake_started`.
        let tls_id = crate::servlet::RUSTLS_SOCK_ID_BASE + stream_id;

        // Build an SSLSocket wrapper. Reuses the existing SSLSocket/
        // SSLSocketInputStream/SSLSocketOutputStream classes.
        // gcd d1/e: plus the handshake-listener slot
        // (`ssl_security::alloc_ssl_socket`).
        let sock = crate::phases_late::ssl_security::alloc_ssl_socket(ctx, SSS_SOCK_FIELDS)?;
        // E12: a MISS here means the registry has no record of this stream, so
        // there is nothing this VM can report about what it negotiated. The old
        // pair announced the VM's default TLS version for a stream it could not
        // find. Answer JSSE's own "nothing negotiated" spelling instead — see
        // `phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE`.
        let (proto, cipher, alpn, sni) = rustls_session_info(stream_id).unwrap_or_else(|| {
            (
                crate::phases_late::ssl_security::JSSE_NULL_PROTOCOL.into(),
                crate::phases_late::ssl_security::JSSE_NULL_CIPHER_SUITE.into(),
                None,
                None,
            )
        });
        // PIN across every allocation below. `create_string` and
        // `alloc_concurrent_synthetic` can each run a moving young collection,
        // which relocates `sock` — after which the raw `set_field` writes, the
        // ALPN stash and the returned reference would all address the old
        // address. `new13_finish_socket` pins for exactly this reason.
        let sock_pin = ctx.pin_native_root(sock);
        let host_str = ctx.create_string(sni.as_deref().unwrap_or("server"));
        let sock = ctx.read_native_pin(sock_pin, sock);
        crate::phases_late::ssl_security::sock_set_field(
            &*ctx,
            sock,
            SSS_SOCK_HOST,
            Value::Object(Some(host_str)),
        );
        crate::phases_late::ssl_security::sock_set_field(&*ctx, sock, SSS_SOCK_PORT, Value::Int(0));
        crate::phases_late::ssl_security::sock_set_field(
            &*ctx,
            sock,
            SSS_SOCK_TLSID,
            Value::Int(tls_id),
        );
        crate::phases_late::ssl_security::sock_set_field(
            &*ctx,
            sock,
            SSS_SOCK_CLOSED,
            Value::Int(0),
        );
        // FIX (sslserversocket-accept-stream-id): the raw field writes above
        // are NOT enough, and on this JDK they do nothing at all.
        // `javax/net/ssl/SSLSocket` is a real loaded class, so
        // `alloc_concurrent_synthetic` gives the object the REAL layout, whose
        // field #2 is reference-typed — the field-layout guard silently drops a
        // mismatched Int write there. `new13_resolve_tls_id` then reads back
        // `Object(None)`, falls through to `net_phase_e`'s side table, finds
        // nothing, and answers -1. Every I/O method on the accepted socket
        // reads that -1 as "closed": the server half of a plain in-process
        // `SSLServerSocket` echo failed with `SSLSocketOutputStream.write:
        // stream is closed` before moving a byte, while the same code passed on
        // HotSpot.
        //
        // The client side already learned this (`new13_finish_socket`'s
        // netty-client-socket-write-after-close comment) and records the
        // authoritative id in the side table. The accept path never did, which
        // is why the "unreliable blocking read/write on the accepted socket"
        // noted here previously was never reproducible as anything else.
        let endpoint = with_raw_tls_stream(stream_id, |tcp| {
            (tcp.local_addr().ok(), tcp.peer_addr().ok())
        });
        let (local_port, peer_port) = endpoint
            .map(|(l, p)| {
                (
                    l.map(|a| i32::from(a.port())).unwrap_or(0),
                    p.map(|a| i32::from(a.port())).unwrap_or(0),
                )
            })
            .unwrap_or((0, 0));
        crate::net_phase_e::sock_set_for_create_with_local_port(
            ctx, sock, peer_port, local_port, tls_id,
        );
        let sock = crate::net_phase_e::re1_init_socket_locks(ctx, sock)?;
        let sock_key = gc_stable_objref_key(ctx, sock);
        sss_mode_states().lock().insert(sock_key, accepted_mode);
        crate::net_phase_e::sock_set(ctx, sock, |s| {
            s.enabled_protocols = Some(accepted_protocols)
        });

        // 4-field synthetic session: proto, cipher, streamId, attrs (slot 3 —
        // see `sslsess_attrs_slot`, which is the width->slot table; this row
        // and the 8-field one are the only two with a dedicated attrs slot).
        // E31: these two comments used to name `SSLSESS_ATTRS_SLOT`, a doc
        // comment on a constant that has never existed in this tree — the
        // width rule they pointed at was only ever `num_fields - 1` open-coded
        // at five call sites. It exists now.
        let session0 = alloc_session_carrier(ctx, 4)?;
        // gc-common w11-a: a new session object starts with no side-table rows
        // (see `forget_session_rows`).
        forget_session_rows(ctx, session0);
        // GC: the four lines this replaces held `session` — and `p` — raw
        // across `create_string`, which can run a moving young collection. The
        // session's own pin is taken AFTER `sock_pin` so the single
        // `unpin_native_roots(sock_pin)` at the end of this body still
        // truncates both; the same discipline the ALPN stash and
        // `new13_finish_socket` already use here.
        let session_pin = ctx.pin_native_root(session0);
        let p = ctx.create_string(&proto);
        let session = ctx.read_native_pin(session_pin, session0);
        sess_set(&*ctx, session, 0, Value::Object(Some(p)));
        let c = ctx.create_string(&cipher);
        let session = ctx.read_native_pin(session_pin, session0);
        sess_set(&*ctx, session, 1, Value::Object(Some(c)));
        // Offset id here too: the session accessors subtract
        // `RUSTLS_SOCK_ID_BASE` before asking `rustls_session_info`, and pass
        // anything below it to the native-tls lookup instead.
        sess_set(&*ctx, session, 2, Value::Int(tls_id));
        let sock = ctx.read_native_pin(sock_pin, sock);
        crate::phases_late::ssl_security::sock_set_field(
            &*ctx,
            sock,
            SSS_SOCK_SESSION,
            Value::Object(Some(session)),
        );
        // G51 — the two facts this session carries that its four slots have no
        // room for, both keyed on the session object.
        //
        // MEASURED, `RSslLiveSession` on `9ae371468`: `server.peerPort.isPositive
        // = false WANT true`, and `server.localPrincipal` / `.class` /
        // `server.localCertificates.length` all answering "no local identity"
        // for a server that had just proved one. The endpoint is the CLIENT's
        // — the server's peer is the client, so this is an ephemeral port and
        // NOT the listener's; see `session_peer_endpoint_table` for the whole
        // measured family. `rustls_session_info`'s SNI is deliberately not used
        // as the host: HotSpot answers the peer's address literal here even
        // when a different SNI name was sent, measured in both directions.
        if let Some((peer_host, peer_port)) = rustls_server_peer_endpoint(stream_id) {
            record_session_peer_endpoint(ctx, session, &peer_host, peer_port);
        }
        // What the client asked for as `server_name`: `getRequestedServerNames()` answers it.
        record_session_sni(&*ctx, session, sni.clone(), true);
        if let Some(chain) = rustls_server_peer_cert_chain_der(stream_id) {
            record_client_peer_chain(&*ctx, session, chain);
        }

        // The listener's own certificate chain — what this side sent the peer,
        // which is what `getLocalCertificates()`/`getLocalPrincipal()` answer.
        // Read from `sss_listener_identities`, whose row is inserted by
        // `create_ssl_server_socket` from the very identity the rustls
        // `ServerConfig` above was built out of, so the chain reported here and
        // the chain presented on the wire have one source.
        let local_pem = sss_listener_identities()
            .lock()
            .get(&id)
            .map(|identity| identity.cert_pem.clone());
        if let Some(pem) = local_pem {
            let chain: Vec<Vec<u8>> = parse_cert_chain_pem(&pem)
                .map(|certs| certs.iter().map(|c| c.as_ref().to_vec()).collect())
                .unwrap_or_default();
            record_local_cert_chain(ctx, session, chain);
        }
        // Stash ALPN on the socket so `getApplicationProtocol()` can read it.
        // We use a side-table rather than widening SSLSocket's shape.
        if let Some(alpn_str) = alpn {
            stash_sock_alpn(ctx, sock, alpn_str);
        }
        ctx.unpin_native_roots(sock_pin);
        Ok(Some(Value::Object(Some(sock))))
    }, cratonvm_native_api::NativeKind::Bridge);
    // bind(SocketAddress) — STUB-REMOVAL (wave 3). Was `Ok(None)`.
    //
    // Every `javax/net/ssl/SSLServerSocket` this module hands out is created
    // ALREADY BOUND: all three `SSLServerSocketFactory.createServerSocket`
    // overloads (~4341/4354/4363) open the rustls `TcpListener` up front, and
    // there is no unbound-construction path. `ServerSocket.bind` on an already
    // bound socket is specified to throw ("if the socket is already bound"),
    // and on a closed socket to throw `SocketException: Socket is closed`.
    //
    // The old no-op let a caller rebind to a different address/port and get
    // silence, while `getLocalPort()`/`accept()` kept serving the ORIGINAL
    // listener — the address the caller asked for was never listened on. Any
    // caller this now throws for is a caller that would also have thrown on a
    // real JVM.
    r.register(sss, "bind", "(Ljava/net/SocketAddress;)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let closed = ssl_server_socket_state(ctx, this)
            .map(|state| state.closed)
            .unwrap_or(ssl_server_socket_state_miss().closed);
        if closed != 0 {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "Socket is closed",
            ));
        }
        // An UNBOUND socket from the no-arg `createServerSocket()` overload
        // is the one case that is not already bound — and this file cannot
        // bring a listener up for it: `create_ssl_server_socket` resolves its
        // TLS identity from the FACTORY it was called on, and a socket keeps
        // no rooted reference to that factory. Refuse loudly rather than bind
        // a plaintext listener behind an `SSLServerSocket`, which is what the
        // inherited `ServerSocketFactory` native used to hand out here.
        if ssl_server_socket_state(ctx, this).is_some_and(|state| state.bound == 0) {
            return Err(crate::phases_early::throw_jca_exc(
                ctx,
                "java/net/SocketException",
                "binding an unbound SSLServerSocket is not implemented; \n                 use SSLServerSocketFactory.createServerSocket(int)",
            ));
        }
        Err(crate::phases_early::throw_jca_exc(
            ctx,
            "java/net/SocketException",
            "Already bound",
        ))
    }, cratonvm_native_api::NativeKind::Bridge);
    // STUB-REMOVAL (wave 2): both were `Ok(None)` no-ops — see
    // `sss_listener_identities` for why a silently-ignored
    // `setNeedClientAuth(true)` is the worst failure mode in this file. These
    // now rebuild the listener's rustls `ServerConfig` with a real
    // `WebPkiClientVerifier` (mandatory for `need`, `allow_unauthenticated`
    // for `want`) and throw when that cannot be done, so a server never
    // believes mTLS is enforced when it is not.
    r.register(sss, "setNeedClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        // JSSE: setNeedClientAuth(true) clears wantClientAuth.
        sss_apply_client_auth(ctx, this, on, false)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "setWantClientAuth", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = args.get(1).and_then(|v| v.as_int()).unwrap_or(0) != 0;
        sss_apply_client_auth(ctx, this, false, on)
    }, cratonvm_native_api::NativeKind::Bridge);
    // Paired getters: `javax.net.ssl.SSLServerSocket` declares both abstract,
    // so without a native an ordinary read-back throws AbstractMethodError.
    // Now that the setters keep real state, report it.
    r.register(sss, "getNeedClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = existing_objref_key(ctx, this);
        let need = key
            .and_then(|key| sss_client_auth_states().lock().get(&key).copied())
            .map(|(need, _)| need)
            .unwrap_or(0);
        Ok(Some(Value::Int(need)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "getWantClientAuth", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = existing_objref_key(ctx, this);
        let want = key
            .and_then(|key| sss_client_auth_states().lock().get(&key).copied())
            .map(|(_, want)| want)
            .unwrap_or(0);
        Ok(Some(Value::Int(want)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // getEnabledProtocols/setEnabledProtocols — `javax.net.ssl.SSLServerSocket`
    // is a real, abstract JDK class; unlike `SSLSocket`/`SSLEngine` (whose
    // `cls_impl` natives cover these via the shared `with_engine` state),
    // `SSLServerSocket` had no registration for either at all, so any caller
    // that round-trips through them (H2's `CipherFactory.createServerSocket`
    // calls `setEnabledProtocols(disableSSL(getEnabledProtocols()))`
    // immediately after construction) hit `AbstractMethodError: ... has no
    // Code attribute` — the interpreter found no native and no concrete
    // bytecode to fall back to. Backed by the same
    // `gc_stable_objref_key`-indexed side-table pattern as `sock_alpn_table`.
    r.register(
        sss,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
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
            // Same rule as the `SSLEngineImpl` setter — see its comment.
            if list.is_empty() && given > 0 {
                list = vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()];
            }
            stash_sss_enabled_protocols(ctx, this, list);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sss,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = lookup_sss_enabled_protocols(ctx, this)
                .unwrap_or_else(|| vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]);
            // Typed and rooted — see `materialize_java_string_array`.
            let arr = materialize_java_string_array(ctx, &list);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sss,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            // Typed and rooted — see `materialize_java_string_array`.
            let arr = materialize_java_string_array(ctx, &["TLSv1.3", "TLSv1.2"]);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );

    // ------------------------------------------------------------------
    // G25 — the four BIND-IDENTITY rows.
    //
    // They land TOGETHER or not at all, and the reason is not tidiness.
    // `java.net.ServerSocket.toString()` is
    //
    //     if (!isBound()) return "ServerSocket[unbound]";
    //     return "ServerSocket[" + impl.toString() + "]";
    //
    // and `isBound()` there is an `invokevirtual` on `this`, so it finds a
    // native registered on the receiver's class. Registering `isBound` alone
    // — the obvious one-row fix, and the oracle does say `true` — therefore
    // walks the inherited `toString()` straight into `impl.toString()` on a
    // null `impl`, converting a row that AGREED with HotSpot into an NPE.
    // `net_phase_e`'s
    // `ssl_server_socket_bound_identity_rows_are_not_registered_piecemeal`
    // is the tripwire on that file's side; `sss_bind_identity_rows_move_together`
    // in this file's `mod tests` is the tripwire on this one's.
    //
    // MEASURED, HotSpot 25.0.3+9 (`G25Probe`), on a socket bound to
    // 127.0.0.1 and then CLOSED — every row below is unchanged by `close()`,
    // which is why `close()` carries the bind identity forward:
    //
    //     isBound                = true                      (open and closed)
    //     getInetAddress         = /127.0.0.1                (open and closed)
    //     getLocalSocketAddress  = /127.0.0.1:<port>         (open and closed)
    //     toString               = [SSL: ServerSocket[addr=/127.0.0.1,localport=<port>]]
    //
    // and on a WILDCARD-bound one, where the rendering differs and is the
    // reason `SSS_WILDCARD_DISPLAY` exists:
    //
    //     getInetAddress         = 0.0.0.0/0.0.0.0
    //     toString               = [SSL: ServerSocket[addr=0.0.0.0/0.0.0.0,localport=<port>]]
    //
    // Note the `[SSL: ...]` wrapper: `sun.security.ssl.SSLServerSocketImpl`
    // OVERRIDES `toString()`, so the plain `ServerSocket[...]` form an earlier
    // nomination predicted is not what the oracle prints.
    r.register(sss, "isBound", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        // "We have a record of it" is no longer "it is bound": the NO-ARG
        // `createServerSocket()` overload records an UNBOUND socket, which is
        // the whole reason `SslServerSocketState::bound` exists. A miss is
        // still `false`.
        Ok(Some(Value::Int(
            ssl_server_socket_state(ctx, this)
                .map(|state| state.bound)
                .unwrap_or(0),
        )))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(
        sss,
        "getInetAddress",
        "()Ljava/net/InetAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(state) = ssl_server_socket_state(ctx, this) else {
                return Ok(Some(Value::Object(None)));
            };
            let address = match sss_local_socket_address(ctx, &state)? {
                Some(Value::Object(Some(address))) => address,
                _ => return Ok(Some(Value::Object(None))),
            };
            let pin = ctx.pin_native_root(address);
            let address = ctx.read_native_pin(pin, address);
            let result = ctx.invoke_virtual(address, "getAddress", "()Ljava/net/InetAddress;", &[]);
            ctx.unpin_native_roots(pin);
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sss,
        "getLocalSocketAddress",
        "()Ljava/net/SocketAddress;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let Some(state) = ssl_server_socket_state(ctx, this) else {
                // `ServerSocket.getLocalSocketAddress()` answers null, not an
                // exception, when the socket is not bound.
                return Ok(Some(Value::Object(None)));
            };
            sss_local_socket_address(ctx, &state)        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(sss, "toString", "()Ljava/lang/String;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let text = sss_to_string(ssl_server_socket_state(ctx, this).as_ref());
        let text = ctx.create_string(&text);
        Ok(Some(Value::Object(Some(text))))
    }, cratonvm_native_api::NativeKind::Bridge);

    // ------------------------------------------------------------------
    // G25 — the three rows that were `AbstractMethodError`.
    //
    // `javax.net.ssl.SSLServerSocket` is ABSTRACT and the object is an
    // instance of it directly, so a method with no native and no concrete
    // body raises `AbstractMethodError: ... has no Code attribute`. MEASURED
    // on both VMs (`G16Sweep`) for `getEnabledCipherSuites`,
    // `getUseClientMode` and `getEnableSessionCreation`.
    //
    // The setters come with them. Each of these is a settable JSSE property
    // whose getter is a read-back, MEASURED (`G25Probe`):
    //
    //     getUseClientMode          = false, then true after setUseClientMode(true)
    //     getEnableSessionCreation  = true,  then false after set...(false)
    //     getEnabledCipherSuites   == getSupportedCipherSuites until set,
    //                                 then exactly what was set
    //     both survive close()
    //
    // Registering only the getters would have made them constants that lie
    // the moment anybody calls a setter — and the setter would still have
    // been an `AbstractMethodError`.
    r.register(
        sss,
        "getSupportedCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            Ok(Some(Value::Object(Some(
                crate::phases_late::ssl_security::jsse_supported_suite_name_array(ctx)?,
            ))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sss,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // MEASURED: on HotSpot `getEnabledCipherSuites()` equals
            // `getSupportedCipherSuites()` element-wise until somebody narrows
            // it — enabled == default == supported, one list (see
            // `jsse_supported_suite_name_array`'s comment, which measured the
            // same identity on the socket factory). So the unset answer is the
            // supported array, not a narrower invented "defaults" set.
            let Some(list) = lookup_sss_enabled_suites(ctx, this) else {
                return Ok(Some(Value::Object(Some(
                    crate::phases_late::ssl_security::jsse_supported_suite_name_array(ctx)?,
                ))));
            };
            // Typed and rooted — see `materialize_java_string_array`.
            let arr = materialize_java_string_array(ctx, &list);
            Ok(Some(Value::Object(Some(arr))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        sss,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Both messages TRANSCRIBED from HotSpot 25.0.3+9 (`G25Probe`),
            // not derived — HANDOFF §5. The null check comes first there, and
            // the order is observable: `setEnabledCipherSuites(null)` reports
            // "CipherSuites cannot be null", never "Unsupported CipherSuite:
            // null".
            let Some(Value::Object(Some(arr))) = args.get(1) else {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "CipherSuites cannot be null".into(),
                }
                .into());
            };
            let mut list: Vec<String> = Vec::new();
            for i in 0..ctx.array_length(*arr) {
                let name = match ctx.get_array_element(*arr, i) {
                    Value::Object(Some(name)) => ctx.read_string(name).unwrap_or_default(),
                    _ => String::new(),
                };
                if !is_cipher_suite_name(&name) {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: format!("Unsupported CipherSuite: {name}"),
                    }
                    .into());
                }
                list.push(name);
            }
            stash_sss_enabled_suites(ctx, this, list);
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(sss, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sss_mode_state(ctx, this).0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        // NOTE: this records the flag and does NOT turn the listener round.
        // `accept()` still performs a SERVER handshake. Real JSSE would make
        // the accepted socket start in client mode; nothing in this VM's
        // rustls listener can do that, and pretending otherwise would be the
        // silently-disabled-check shape `sss_apply_client_auth` exists to
        // avoid. Recorded so the read-back is honest about what was asked;
        // see the record's NOMINATION.
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert(SSS_MODE_DEFAULT);
        entry.0 = on;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "getEnableSessionCreation", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(Value::Int(sss_mode_state(ctx, this).1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(sss, "setEnableSessionCreation", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert(SSS_MODE_DEFAULT);
        entry.1 = on;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    }

/// `(getUseClientMode, getEnableSessionCreation)` on a socket nobody has
/// configured. MEASURED on HotSpot 25.0.3+9 (`G25Probe`): a server socket is
/// not in client mode, and session creation is on.
pub const SSS_MODE_DEFAULT: (i32, i32) = (0, 1);

/// Per-`SSLServerSocket` `(useClientMode, enableSessionCreation)`, keyed by
/// [`gc_stable_objref_key`]. Same rationale as `sss_client_auth_states`: the
/// real `java.net.ServerSocket` layout has no slot that means either of these
/// (see [`SSS_FIELDS`]), and writing into one that means something else is the
/// defect this whole block exists to undo.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Every acquisition takes
/// the guard after `gc_stable_objref_key` has already produced the key, and
/// holds it only across a `HashMap::entry` on a `(i32, i32)`.
pub fn sss_mode_states() -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (i32, i32)>>
{
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, (i32, i32)>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

pub fn sss_mode_state(ctx: &dyn NativeContext, socket: ObjectRef) -> (i32, i32) {
    sss_mode_state_or(ctx, socket, SSS_MODE_DEFAULT)
}

/// `socket`'s recorded `(useClientMode, enableSessionCreation)`, or `default`
/// when nothing was recorded. A lookup: never mints a key (gc-common w26-b).
pub fn sss_mode_state_or(
    ctx: &dyn NativeContext,
    socket: ObjectRef,
    default: (i32, i32),
) -> (i32, i32) {
    let key = match existing_objref_key(ctx, socket) {
        Some(key) => key,
        None => return default,
    };
    sss_mode_states()
        .lock()
        .get(&key)
        .copied()
        .unwrap_or(default)
}

/// Per-`SSLServerSocket` `setEnabledCipherSuites` list. Absent means "never
/// narrowed", which reads back as the full supported list — not as an empty
/// one, which would say this socket can negotiate nothing.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions are one
/// statement over an already-built key and an already-built `Vec<String>`.
pub fn sss_enabled_suites_table(
) -> &'static cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<String>>> {
    static T: OnceLock<cratonvm_types::lock_order::OrderedPlMutex<HashMap<u64, Vec<String>>>> =
        OnceLock::new();
    T.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            HashMap::new(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

pub fn stash_sss_enabled_suites(ctx: &dyn NativeContext, socket: ObjectRef, suites: Vec<String>) {
    let key = gc_stable_objref_key(ctx, socket);
    sss_enabled_suites_table().lock().insert(key, suites);
}

pub fn lookup_sss_enabled_suites(ctx: &dyn NativeContext, socket: ObjectRef) -> Option<Vec<String>> {
    let key = existing_objref_key(ctx, socket)?;
    sss_enabled_suites_table().lock().get(&key).cloned()
}

/// A fresh `java.net.InetSocketAddress` for this listener's bind address and
/// local port — the object `getLocalSocketAddress()` returns and the one
/// `getInetAddress()` unwraps.
///
/// Built through the REAL `InetSocketAddress` constructor bytecode rather than
/// transcribed, so the `hostname/literal:port` rendering, the IPv6 bracketing
/// and the resolved/unresolved distinction are the JDK's own.
pub fn sss_local_socket_address(
    ctx: &mut dyn NativeContext,
    state: &SslServerSocketState,
) -> Result<Option<Value>, MethodCallFailed> {
    if state.bind_address == SSS_WILDCARD_BIND {
        // The `(int)` constructor, deliberately — see `SSS_WILDCARD_DISPLAY`.
        // `InetSocketAddress("0.0.0.0", port)` renders `/0.0.0.0:port`, and
        // the oracle renders `0.0.0.0/0.0.0.0:port`.
        return ctx.new_object_initialized(
            "java/net/InetSocketAddress",
            "(I)V",
            &[Value::Int(state.local_port)],
        );
    }
    let host = ctx.create_string(&state.bind_address);
    // The constructor runs real bytecode (it calls `InetAddress.getByName`),
    // so it allocates and can relocate `host` — the same Family-1 shape the
    // `java.net.Socket.getRemoteSocketAddress` sites in `phases_early` pin for.
    let pin = ctx.pin_native_root(host);
    let host = ctx.read_native_pin(pin, host);
    let result = ctx.new_object_initialized(
        "java/net/InetSocketAddress",
        "(Ljava/lang/String;I)V",
        &[Value::Object(Some(host)), Value::Int(state.local_port)],
    );
    ctx.unpin_native_roots(pin);
    result
}

/// The `SO_TIMEOUT` `SSLServerSocket.accept()` must honour, as a `Duration`.
///
/// The value lives on `net_phase_e`'s RE.6b delegate — a real, unbound
/// `java.net.ServerSocket` that `setSoTimeout` forwards to — so it is asked
/// for through the Java door rather than through a cross-file accessor. That
/// keeps ONE owner for the value: this file must not register `getSoTimeout`
/// itself, because `register_t27_natives` runs after
/// `register_phase_e_networking` and would silently kill all nine of RE.6b's
/// bodies.
///
/// **Dropping the error is safe here, and that is a property of this VM's
/// calling convention rather than an assumption.** `MethodCallFailed
/// ::ExceptionThrown` carries the `Throwable` BY VALUE; there is no VM-global
/// pending-exception slot left dirty by discarding it. So if RE.6b is ever
/// removed and the inherited `ServerSocket.getSoTimeout()` bytecode runs into
/// the null `impl`, `accept()` keeps its pre-G25 behaviour — block forever —
/// instead of acquiring a new way to fail.
pub fn sss_accept_timeout(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<std::time::Duration> {
    let pin = ctx.pin_native_root(this);
    let answer = ctx.invoke_virtual(this, "getSoTimeout", "()I", &[]);
    ctx.unpin_native_roots(pin);
    // 0 is `java.net.ServerSocket`'s spelling of "no timeout", and it is also
    // what an un-configured socket reports, so it must not become a
    // zero-length deadline that times out instantly.
    answer
        .ok()
        .flatten()
        .and_then(|value| value.as_int())
        .filter(|millis| *millis > 0)
        .map(|millis| std::time::Duration::from_millis(millis as u64))
}

/// Side-table storing this `SSLServerSocket`'s `setEnabledProtocols` list.
/// Same rationale/keying as `sock_alpn_table` (see `gc_stable_objref_key`'s
/// doc comment) — `SSLServerSocket`'s synthetic 4-field layout has no spare
/// slot for a `String[]`.
pub fn sss_enabled_protocols_table() -> &'static Mutex<HashMap<u64, Vec<String>>> {
    static T: OnceLock<Mutex<HashMap<u64, Vec<String>>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn stash_sss_enabled_protocols(ctx: &dyn NativeContext, socket: ObjectRef, protocols: Vec<String>) {
    let key = gc_stable_objref_key(ctx, socket);
    sss_enabled_protocols_table().lock().insert(key, protocols);
}

pub fn lookup_sss_enabled_protocols(ctx: &dyn NativeContext, socket: ObjectRef) -> Option<Vec<String>> {
    let key = existing_objref_key(ctx, socket)?;
    sss_enabled_protocols_table().lock().get(&key).cloned()
}

/// Side-table storing ALPN protocols per SSLSocket objectref. Used so
/// `SSLSocket.getApplicationProtocol()` can return real values without
/// widening the synthetic-field layout (which would break every
/// SSLSocket allocation site in phases_late).
pub fn sock_alpn_table() -> &'static Mutex<HashMap<u64, String>> {
    static T: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub fn stash_sock_alpn(ctx: &dyn NativeContext, sock: ObjectRef, alpn: String) {
    let key = gc_stable_objref_key(ctx, sock);
    sock_alpn_table().lock().insert(key, alpn);
}

pub fn lookup_sock_alpn(ctx: &dyn NativeContext, sock: ObjectRef) -> Option<String> {
    let key = existing_objref_key(ctx, sock)?;
    sock_alpn_table().lock().get(&key).cloned()
}

/// GC-stable identity key for a Java-object-keyed side table.
///
/// FIX (tomcat-t27-tls-side-table-objref-key-instability): this used to hash
/// the `ObjectRef`'s Debug-formatted raw pointer value (`objref_key`, now
/// removed). `ObjectRef` is a bare pointer to a heap object, and this VM's
/// young-gen GC moves/reclaims objects, so a live object's `ObjectRef` is not
/// a stable identity across its own lifetime (if it moves) and a
/// *different*, unrelated object can later be allocated at the same address
/// once the original is collected — a table lookup can then silently
/// *collide* with a stale entry for a completely different, already-freed
/// object (a wrong-identity match, not just a miss: the table's `Some`
/// result is trusted over any field-based fallback). This is the exact same
/// architectural defect already fixed once in this file for
/// `engine_table`/`sslparams_alpn_table` via `engine_objref_key` (see its
/// doc comment, and
/// `reactive-httpcomponents-connector-flaky-tls-engine-identity-and-pool-cipher-leak-FIXED.md`)
/// — that earlier fix's scope note explicitly left
/// `ssl_server_socket_states`, `sock_alpn_table`, `session_peer_certs_table`,
/// and `SSLSession.getId()`'s seed unfixed; this closes those.
///
/// gc-common w26-b (`common-w25a-t27-tls-engine-rows-merge-live-identity-hash-colliders`):
/// the key is now the object's WEAK LOCK KEY (`crate::gc_stable_weak_lock_key`),
/// no longer `(VM, identity hash)`. An identity hash is not unique -- it is
/// 32 bits, minted from one per-heap counter that wraps -- so two LIVE
/// engines / sockets / sessions of one VM could share every row (the second
/// engine drove the first one's rustls connection), and a new engine's
/// mint-time reset wiped a live predecessor's state. A weak lock key is per
/// VM and per LIVE object (matched by the object's current address, which the
/// stop-the-world epilogue re-addresses after a move), stable for the
/// object's whole life, never minted again, and freed by the first lock-key
/// sweep that finds the object dead; that sweep then drops every row filed
/// under it ([`forget_tls_object_keys`], called from `lib.rs::sweep_lock_keys`
/// and `forget_vm_lock_keys`). The owning VM is the registry's
/// ([`objref_key_owned_by`]).
///
/// This MINTS a slot: call it where a row is filed (or where the key must be
/// stable, as `getId()`'s seed). A pure lookup uses [`existing_objref_key`],
/// which never leaves a slot behind for an object that has no row. Either
/// takes the lock-key registry, a leaf a sweep also takes, so compute the key
/// BEFORE taking one of this module's table locks.
pub fn gc_stable_objref_key(ctx: &dyn NativeContext, o: ObjectRef) -> u64 {
    crate::gc_stable_weak_lock_key(ctx, o)
        .map(|k| k as u64)
        .unwrap_or_else(|_| unreachable!("gc_stable_weak_lock_key never fails"))
}

/// The key [`gc_stable_objref_key`] would answer for `o`, or `None` when the
/// calling VM never keyed `o` -- in which case no table of this module has a
/// row for it. Never mints (gc-common w26-b).
#[inline]
pub fn existing_objref_key(ctx: &dyn NativeContext, o: ObjectRef) -> Option<u64> {
    crate::existing_weak_lock_key(ctx, o).map(|k| k as u64)
}

/// Whether an object key ([`gc_stable_objref_key`]) belongs to VM `vm`: the
/// lock-key registry's answer. A key whose slot is gone (its object died and
/// the sweep freed it, or its VM was torn down) belongs to nobody; its rows
/// are dropped by [`forget_tls_object_keys`] when the key is freed, and
/// [`forget_vm_tls_state`] runs before `forget_vm_lock_keys`.
#[inline]
pub fn objref_key_owned_by(key: u64, vm: usize) -> bool {
    crate::lock_key_owner_vm(key) == Some(vm)
}

/// Whether an `SSLContext` key minted by [`ctx_obj_key`] belongs to VM `vm`.
/// A key whose minting entry is gone (never minted here, or its VM was torn
/// down) counts as everyone's, so a scan can only over-retain.
#[inline]
pub fn ctx_key_owned_by(key: u64, vm: usize) -> bool {
    crate::lock_key_owner_vm(key).map_or(true, |owner| owner == vm)
}

/// Whether the CALLING VM may consult the objects filed under `SSLContext`
/// key `key`. A process-wide slot (the `HttpsURLConnection` defaults) can
/// still hand VM B a key VM A minted; its `TrustManager` / `KeyManager`
/// objects live in A's heap and must not be invoked from B (gc-common w9-b).
#[inline]
pub fn ctx_key_visible_to(ctx: &dyn NativeContext, key: u64) -> bool {
    ctx_key_owned_by(key, ctx.vm_identity())
}

/// The client-socket half of the G25 fix.
///
/// `javax.net.ssl.SSLSocket` is abstract exactly like `SSLServerSocket`, and
/// this VM allocates instances of it directly (`try_alloc_concurrent_synthetic`
/// a few hundred lines above), so any method with no native and no concrete
/// body raises `AbstractMethodError: ... has no Code attribute` rather than
/// answering. G25 fixed the SERVER socket's three; the client socket kept
/// them, and `L6TlsParamSweep` row 77 is the one that surfaced:
///
/// ```text
///   HotSpot   connected=false clientMode=true need=false want=false createSessions=true …
///   CratonVM  THREW java.lang.AbstractMethodError msg=method
///             javax/net/ssl/SSLSocket.getEnableSessionCreation()Z has no Code attribute
/// ```
///
/// One throw takes the whole row with it, so the four properties printed
/// beside it were unobservable too.
///
/// The two validating setters come with it for the same reason they came with
/// the server socket's: a setter that accepts an unsupported suite silently is
/// a configuration error the caller never hears about, and the messages are
/// transcribed from HotSpot rather than invented.
pub fn register_client_socket_mode_accessors(r: &mut NativeMethodRegistry) {
    let ss = "javax/net/ssl/SSLSocket";

    // A client socket IS in client mode, and session creation is on: the
    // mirror image of `SSS_MODE_DEFAULT`, whose first element is 0 because a
    // SERVER socket is not.
    r.register(ss, "getEnableSessionCreation", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = sss_mode_state_or(ctx, this, (1, 1));
        Ok(Some(Value::Int(state.1)))
    }, cratonvm_native_api::NativeKind::Bridge);
    // A socket from `SSLSocketFactory.createSocket()` IS in client mode:
    // measured `clientMode=true` on HotSpot where this VM answered false,
    // because the only `getUseClientMode` registered was the SERVER socket's
    // and its default is the opposite.
    r.register(ss, "getUseClientMode", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state = sss_mode_state_or(ctx, this, (1, 1));
        Ok(Some(Value::Int(state.0)))
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "setUseClientMode", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        let tls_id = crate::phases_late::ssl_security::new13_resolve_tls_id(ctx, this);
        if tls_id >= crate::servlet::PENDING_LAYERED_SOCK_ID_BASE
            && tls_id < crate::servlet::RUSTLS_SOCK_ID_BASE
        {
            let pending_id = tls_id - crate::servlet::PENDING_LAYERED_SOCK_ID_BASE;
            set_pending_layered_socket_client_mode(pending_id, on != 0);
        }
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert((1, 1));
        entry.0 = on;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);
    r.register(ss, "setEnableSessionCreation", "(Z)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let on = i32::from(args.get(1).and_then(|value| value.as_int()).unwrap_or(0) != 0);
        let key = gc_stable_objref_key(ctx, this);
        let mut table = sss_mode_states().lock();
        let entry = table.entry(key).or_insert((1, 1));
        entry.1 = on;
        Ok(None)
    }, cratonvm_native_api::NativeKind::Bridge);

    // The validation lives in the shared bodies, which ALSO apply the
    // restriction. This registrar runs after `net_phase_e`'s and wins, and the
    // first cut of it validated and then returned -- which silently dropped
    // every client cipher/protocol restriction in the VM (Tomcat
    // `TestSSLHostConfig{Cipher,Protocol,Compat}`). See
    // `net_phase_e::ssl_socket_set_enabled_cipher_suites`.
    r.register(
        ss,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        crate::net_phase_e::ssl_socket_set_enabled_cipher_suites, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        ss,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        crate::net_phase_e::ssl_socket_set_enabled_protocols, cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn register_alpn_accessor(r: &mut NativeMethodRegistry) {
    // T2.7.11 — SSLSocket.getApplicationProtocol(): returns the ALPN protocol
    // the server picked during the handshake, or the empty string when ALPN
    // was not negotiated (matching the reference JDK). Falls back to the
    // side-table stashed by accept() for server-side sockets, then to
    // servlet::s2_tls_negotiated_alpn for native-tls client sockets.
    r.register(
        "javax/net/ssl/SSLSocket",
        "getApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            if let Some(alpn) = lookup_sock_alpn(ctx, this) {
                return Ok(Some(Value::Object(Some(ctx.create_string(&alpn)))));
            }
            // Client-side: look up via tls_id in native-tls table.
            if ctx.object_num_fields(this) > SSS_SOCK_TLSID {
                let tls_id =
                    crate::phases_late::ssl_security::sock_field(&*ctx, this, SSS_SOCK_TLSID)
                        .as_int()
                        .unwrap_or(-1);
                if tls_id >= 0 {
                    if let Some(alpn) = servlet::s2_tls_negotiated_alpn(tls_id) {
                        let s = ctx.create_string(&alpn);
                        return Ok(Some(Value::Object(Some(s))));
                    }
                    // rustls client tables, too
                    if let Some((_, _, alpn, _)) = rustls_session_info(tls_id) {
                        if let Some(a) = alpn {
                            let s = ctx.create_string(&a);
                            return Ok(Some(Value::Object(Some(s))));
                        }
                    }
                }
            }
            let s = ctx.create_string("");
            Ok(Some(Value::Object(Some(s))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    }

/// `javax.net.ssl.SSLSocket.getHandshakeSession()`.
///
/// **E31 — this registration is what lets `RSslNullSession` reach any of its
/// session assertions at all.** It was registered NOWHERE (confirmed against
/// `--dump-native-registry`, E22-1 §1), and unlike most of the JSSE surface
/// this method is NOT abstract: `javax/net/ssl/SSLSocket` carries a concrete
/// body, and the body is
///
/// ```java
/// // C:\craton\jdk25src/java.base/javax/net/ssl/SSLSocket.java:474-476
/// public SSLSession getHandshakeSession() {
///     throw new UnsupportedOperationException();
/// }
/// ```
///
/// HotSpot never reaches it because `sun.security.ssl.SSLSocketImpl` overrides
/// it; CratonVM's socket **is** a `javax/net/ssl/SSLSocket`, so an
/// un-intercepted call threw where the oracle answers. `getApplicationProtocol`
/// (registered directly above) is the same shape — its base body throws too
/// (`SSLSocket.java:753`, measured) — which is why that one already had a
/// native and this one being absent was easy to miss.
///
/// ## The contract, taken from the override rather than from the base class
///
/// ```java
/// // jdk25src/java.base/sun/security/ssl/SSLSocketImpl.java:384-392
/// public SSLSession getHandshakeSession() {
///     socketLock.lock();
///     try {
///         return conContext.handshakeContext == null ?
///                 null : conContext.handshakeContext.handshakeSession;
///     } finally { socketLock.unlock(); }
/// }
/// ```
///
/// So it is non-null over exactly one window: from the moment a handshake
/// context exists until it is torn down. Measured on HotSpot 25.0.3+9-LTS,
/// `scratchpad/e31/E31HandshakeSessionSocket.java` (loopback, self-signed
/// PKCS12, 3 runs):
///
/// ```text
/// ARM0 abstract-SSLSocket.getHandshakeSession   = THREW UnsupportedOperationException
/// ARMA unconnected.getHandshakeSession          = null
/// ARMA after getSession(), getHandshakeSession  = null
/// ARMA after close(), getHandshakeSession       = null
/// ARMM before startHandshake()                  = null
/// ARMM inside X509ExtendedTrustManager.checkServerTrusted(chain,auth,Socket)
///                                               = SSLSessionImpl{TLS_AES_256_GCM_SHA384,
///                                                 TLSv1.3, id=32B, valid=true}
/// ARMM inside HandshakeCompletedListener        = null   (fires after teardown)
/// ARMF client after startHandshake() returned   = null
/// ARMF server after startHandshake() returned   = null
/// ARMF after close()                            = null
/// ```
///
/// ## Why `null` UNCONDITIONALLY is the right body here, not a lazy one
///
/// The non-null window is real, but it is only *observable* from inside a
/// handshake callback that receives the `Socket` — the 3-arg
/// `X509ExtendedTrustManager.checkServerTrusted(chain, authType, Socket)` /
/// `checkClientTrusted(..., Socket)` overloads, or an `SNIMatcher`. It cannot
/// be observed from another thread, because `getHandshakeSession()` takes the
/// same `socketLock` the handshaking thread is holding; the probe's
/// `HandshakeCompletedListener` arm measures the other edge — by the time the
/// completion notification runs, `handshakeContext` is already null.
///
/// **CratonVM never invokes those overloads.** `engine_run_trust_check` in this
/// file dispatches exactly one descriptor,
/// `([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V` — the 2-arg
/// form, which gets no socket — and there is no `SNIMatcher` callback either.
/// Every socket handshake this VM performs is also synchronous inside
/// `startHandshake()`/`createSocket(host, port)`. So there is no state in which
/// application code can hold a CratonVM `SSLSocket` *and* be inside its
/// handshake, and `null` is HotSpot's answer for every state that is reachable.
/// If the 3-arg trust-manager overload is ever wired up, this body has to grow
/// the `is_handshaking()` gate that `SSLEngineImpl.getHandshakeSession` in this
/// file already carries — that gate is the model to copy, and the reason this
/// comment names the condition instead of just asserting the constant.
///
/// ## Why a native on a class that HAS a bytecode body is reached at all
///
/// Worth stating, because it is the objection that makes this registration
/// look futile: `javax/net/ssl/SSLSocket` is a real, loaded JDK class here, so
/// "the receiver's own class declares the method in bytecode" is true — and
/// `invoke.rs`'s hierarchy walk skips a native when that is true. That skip is
/// in the FALLBACK arm. `resolve_step1_native` runs first and matches the
/// receiver's own class name exactly, which is why every other native this
/// family registers on `javax/net/ssl/SSLSocket` works.
///
/// The witness is in the fixture itself rather than in the dispatcher:
/// `RSslNullSession` DOOR 1's **first** check is `s.isConnected()`, answered by
/// a native registered on exactly this class name, and the vector is recorded
/// as reaching check 2 before aborting — so check 1 was served by the native on
/// a class whose bytecode body exists. Same class, same mechanism.
///
/// One aliasing note for whoever changes this: `invoke.rs` also routes a
/// receiver whose class starts with `sun/security/ssl/SSLSocketImpl` to
/// `javax/net/ssl/SSLSocket`'s registrations. So if a genuinely real
/// `SSLSocketImpl` ever exists in this VM, this body shadows its
/// `conContext.handshakeContext` read too. That is still correct for every
/// state such a socket could be observed in here, for the callback reason
/// above — but it is the assumption to re-check, not a coincidence to rely on.
pub fn register_socket_handshake_session(r: &mut NativeMethodRegistry) {
    r.register(
        "javax/net/ssl/SSLSocket",
        "getHandshakeSession",
        "()Ljavax/net/ssl/SSLSession;",
        |_ctx, _args| Ok(Some(Value::Object(None))), cratonvm_native_api::NativeKind::Bridge,
    );
}

pub fn register_https_url_connection(r: &mut NativeMethodRegistry) {
    // T2.7.14 — javax.net.ssl.HttpsURLConnection. Most of the work here is
    // bookkeeping: the real HTTPS request path lives in `http2.rs`'s
    // `http11_request_impl`, which already drives native-tls against the
    // target URL. These natives expose the JDK's minimal getter/setter
    // surface so Java code compiled against HttpsURLConnection can
    // configure and inspect a connection without tripping over unlinked
    // methods.
    let hurl = "javax/net/ssl/HttpsURLConnection";

    /// The factory `HttpsURLConnection`'s default *and* instance getters both
    /// resolve to when the caller has installed nothing.
    ///
    /// Mint-and-publish, once. The real JDK's
    /// `getDefaultSSLSocketFactory()` assigns to the static
    /// `defaultSSLSocketFactory` on first use, and the constructor seeds every
    /// instance's `sslSocketFactory` from it — so an untouched connection
    /// reports the same object on its first read and forever after. Having
    /// only the *default* getter publish left the instance getter minting a
    /// fresh carrier per call, which
    /// `probes/HucFactoryReadbackProbe.java` catches as an untouched
    /// connection whose factory changes underneath it.
    ///
    /// Publishing into `HUC_DEFAULT_FACTORY` (rather than caching in a
    /// new static) is what keeps a later explicit `setDefaultSSLSocketFactory`
    /// winning, and the slot is already a GC root.
    fn huc_default_factory_or_publish(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
    ) -> Result<ObjectRef, MethodCallFailed> {
        let vm = ctx.vm_identity();
        if let Some(f) = huc_default_ssl_socket_factory_in_vm(vm) {
            return Ok(f);
        }
        let obj = default_ssl_socket_factory_obj(ctx)?;
        set_huc_default_ssl_socket_factory(vm, obj);
        Ok(obj)
    }

    r.register(
        hurl,
        "getDefaultSSLSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            // FIX (tls-handshake-enforcement-gap, doc 21): honour the JDK's
            // documented round trip — once `setDefaultSSLSocketFactory` has
            // published a factory (see `publish_default_ssl_socket_factory`
            // below), return THAT object rather than a fresh, unrelated
            // placeholder. Callers that install a configured factory and later
            // read it back (to wrap it, or to restore it in a test teardown)
            // otherwise silently lost their configuration.
            // FIX (sslsocketfactory-getdefault-aether-resolution-regression-
            // 20260804): this used to mint a BARE 0-field carrier when nothing
            // was published. The JDK documents the unset default as
            // `SSLSocketFactory.getDefault()`, and a caller that takes this
            // factory to the layered
            // `createSocket(Socket,String,int,boolean)` overload reads its
            // field 0 for the owning `SSLContext` — so the bare carrier threw
            // `IllegalStateException: SSLSocketFactory has no owning
            // SSLContext`.
            //
            // FIX (huc-per-connection-ssf-readback): resolving through
            // `huc_default_factory_or_publish` also makes the answer STABLE,
            // which is what the real JDK does here. Measured on JDK 21: two
            // freshly opened connections report the same default-factory
            // identity, even though `SSLSocketFactory.getDefault()` itself
            // returns a new object per call.
            let obj = huc_default_factory_or_publish(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Walk from an arbitrary `SSLSocketFactory`-typed object down to the
    // `SSLContext` it ultimately carries. The fast path is our own synthetic
    // carrier (`try_alloc_concurrent_synthetic("javax/net/ssl/SSLSocketFactory",
    // 1)?`, field 0 = the SSLContext, as returned by `SSLContext.
    // getSocketFactory()`), but real test/application code routinely wraps
    // that in a REAL bytecode subclass that delegates to it — e.g. Tomcat's
    // own `TesterSupport.ClientSSLSocketFactory(SSLSocketFactory delegate)`,
    // used by `TesterSupport.configureClientSsl()` (every `TestCustomSsl`/
    // `TestClientCert*`-style test). That subclass's field 0 is its own
    // `delegate` field — itself another `SSLSocketFactory`, one hop short of
    // the actual `SSLContext` — so blindly reading field 0 once returned the
    // wrapper's delegate and treated IT as the SSLContext. Every
    // `ctx_obj_key`-keyed lookup keyed off the real SSLContext (trust roots,
    // key managers, identity) then silently missed for a completely
    // unrelated object's identity, and the client fell back to the platform
    // default trust store — rejecting the test's self-signed CA with
    // `SSLHandshakeException: ... UnknownIssuer`.
    //
    // Match by `ClassId` (via a single `class_id_by_name` lookup), NOT by
    // `class_name_of_id` on each candidate object: `alloc_concurrent_synthetic`
    // itself documents that `class_name_of_id` can misreport an
    // interface-like synthetic class (this carrier's declared type,
    // `SSLSocketFactory`, is abstract) back as `java/lang/Object` — a
    // name-string comparison at each node silently found nothing and this
    // first attempt returned `None` every time, never actually resolving the
    // wrapped SSLContext. `class_id_by_name` performed ONCE up front and
    // compared by `ClassId` equality is immune to that per-object name
    // misreport. Also explore EVERY reachable Object-typed field (bounded
    // breadth/depth) rather than trying to first guess which one is
    // "SSLSocketFactory-shaped" — that guess is exactly what needed the
    // now-unreliable name check.
    fn resolve_sslcontext_from_factory(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
        factory: ObjectRef,
    ) -> Option<ObjectRef> {
        // `class_num_total_fields` is NOT trustworthy here: our own synthetic
        // `SSLSocketFactory` carrier (`try_alloc_concurrent_synthetic(...,
        // "javax/net/ssl/SSLSocketFactory", 1)?`) reports 0 total fields for
        // its ClassId even though it was allocated with (and, per
        // `get_field`'s M4a contract, safely holds) exactly 1 real slot —
        // confirmed via `CRATONVM_DBG_TLS_AUTH` tracing (`cid=ClassId(1046)
        // nfields=0` for an object that DOES have the SSLContext at index
        // 0). This is the identical "interface-like synthetic class"
        // metadata gap `alloc_concurrent_synthetic` itself documents and
        // works around at allocation time via `num_fields.max(real)` — this
        // resolver hits the same gap on the READ side, where there is no
        // equivalent fallback. `get_field` is required (M4a, this trait's
        // own doc) to bounds-check and fail safe on an out-of-declared-range
        // index, so probing past the end is memory-safe: true out-of-bounds
        // reads come back `Value::Object(None)` and are silently skipped,
        // never a bad memory access.
        //
        // Memory-safe is not the same as free, though, and the unconditional
        // fixed-range scan this used to do was neither silent nor correct as a
        // *slot computation*. `gen_heap::get_field`'s guard classifies the
        // read, and for a receiver whose class layout is fine it takes the arm
        // that says so outright — "caller used slot index past receiver's
        // layout ... the bug is in the caller's slot computation". Probing
        // 0..8 at every node made this resolver that caller: one `TestSsl` run
        // emitted **90** such warnings, all for
        // `TesterSupport$ClientSSLSocketFactory` (`num_slots=3`,
        // `real_field_count=Some(3)`, indices 3..7) — a real bytecode class
        // whose declared layout was available and simply not consulted.
        //
        // So consult it, and keep the fixed range only for the case that
        // actually needs it. `class_num_total_fields` returns 0 both for "no
        // fields" and for "metadata not available" (its own doc: 0 if the
        // class isn't loaded), which is exactly the synthetic-carrier gap
        // above — a carrier holding 1 real slot reports 0. Treating 0 as
        // "unknown, fall back to probing" keeps that path byte-for-byte, while
        // any class that reports a real count is scanned to its own bound and
        // stops generating warnings. A carrier is never missed, because the
        // fallback still covers precisely the objects whose count is unknown.
        const FIELD_SCAN_RANGE: usize = 8;
        const MAX_DEPTH: usize = 6;
        const MAX_VISITED: usize = 64;
        let sslcontext_cid = ctx.class_id_by_name("javax/net/ssl/SSLContext");
        let mut frontier = vec![factory];
        let mut visited = 0usize;
        for _ in 0..MAX_DEPTH {
            let mut next_frontier = Vec::new();
            for obj in frontier {
                if visited >= MAX_VISITED {
                    return None;
                }
                visited += 1;
                let cid = ctx.class_id_of_object(obj);
                if sslcontext_cid == Some(cid) {
                    return Some(obj);
                }
                // Bound the probe by the receiver's OWN declared layout when
                // that layout is known; probe blind only when it is not (see
                // the `FIELD_SCAN_RANGE` comment above).
                let declared = ctx.class_num_total_fields(cid);
                let scan = if declared > 0 {
                    declared
                } else {
                    FIELD_SCAN_RANGE
                };
                for i in 0..scan {
                    if let Value::Object(Some(candidate)) = ctx.get_field(obj, i) {
                        let sub_cid = ctx.class_id_of_object(candidate);
                        if sslcontext_cid == Some(sub_cid) {
                            return Some(candidate);
                        }
                        next_frontier.push(candidate);
                    }
                }
            }
            if next_frontier.is_empty() {
                return None;
            }
            frontier = next_frontier;
        }
        None
    }
    // Capture the client identity (cert+key) carried by the factory's
    // SSLContext so the native HttpsURLConnection client can present a client
    // certificate for mTLS. `setDefaultSSLSocketFactory` is static (factory =
    // args[0]); `setSSLSocketFactory` is instance (factory = args[1]).
    fn capture_huc_client_identity(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
        factory: ObjectRef,
        connection: Option<ObjectRef>,
    ) -> Result<(), MethodCallFailed> {
        if real_jsse_family(&*ctx) {
            // SBR wave 4 (lane SSL): a real JSSE factory holds no carrier
            // `SSLContext`; its trust lives in `SSLContextImpl`. The walk
            // below would find nothing (and used to probe array receivers
            // field by field on the way).
            if let Some(mut impl_ctx) = real_jsse_context_of_factory(ctx, factory) {
                let conn_pin = connection.map(|c| ctx.pin_native_root(c));
                attach_real_jsse_managers(ctx, &mut impl_ctx)?;
                if let (Some(pin), Some(c)) = (conn_pin, connection) {
                    let c = ctx.read_native_pin(pin, c);
                    ctx.unpin_native_roots(pin);
                    capture_huc_ssl_context_for_connection(ctx, c, &mut impl_ctx)?;
                } else {
                    capture_huc_ssl_context(ctx, &mut impl_ctx)?;
                }
                return Ok(());
            }
        }
        if let Some(mut sslctx) = resolve_sslcontext_from_factory(ctx, factory) {
            if let Some(connection) = connection {
                capture_huc_ssl_context_for_connection(ctx, connection, &mut sslctx)?;
            } else {
                capture_huc_ssl_context(ctx, &mut sslctx)?;
            }
        }
        Ok(())
    }
    // FIX (tls-handshake-enforcement-gap, doc 21): this native REPLACES the
    // real `HttpsURLConnection.setDefaultSSLSocketFactory` bytecode, so the
    // real JDK static field `HttpsURLConnection.defaultSSLSocketFactory` was
    // left permanently null. `http_url_connection::
    // huc_upcall_create_socket_if_custom_factory` — the ONLY mechanism that
    // makes a caller-installed `SSLSocketFactory`'s real Java `createSocket`
    // (and therefore its `setEnabledCipherSuites`/`setEnabledProtocols`
    // restrictions) actually run for an `HttpsURLConnection` request — reads
    // exactly that field to find the installed factory, so it silently found
    // nothing and the whole up-call path was dead code. Every Tomcat test
    // that restricts the CLIENT's ciphers or protocols and expects the
    // handshake to fail (`TestSSLHostConfigCipher`, `TestSSLHostConfigCompat`,
    // `TestSSLHostConfigProtocol`) therefore connected unrestricted and
    // succeeded where real JSSE refuses. Publish the factory into the real
    // static field so the reader finds it — and so ordinary Java code calling
    // `getDefaultSSLSocketFactory()` observes the JDK-documented round trip.
    // Writing the real field (rather than caching the `ObjectRef` in a native
    // global) also keeps the factory reachable as a normal static GC root.
    fn publish_default_ssl_socket_factory(
        ctx: &mut dyn cratonvm_native_api::NativeContext,
        factory: ObjectRef,
    ) -> Result<(), MethodCallFailed> {
        // Keep the reference in a GC-rooted native slot. Writing the real JDK
        // static field was tried first and does NOT work: with
        // `CRATONVM_DBG_TLS_AUTH` the very next read reports "default factory
        // is null", so the probe that applies a caller's client-side
        // restriction never ran at all. See `HUC_DEFAULT_FACTORY`.
        set_huc_default_ssl_socket_factory(ctx.vm_identity(), factory);
        // Still attempt the field write, so ordinary Java code reading
        // `HttpsURLConnection.defaultSSLSocketFactory` reflectively sees it if
        // the VM ever starts honouring this.
        let Some(cid) = ctx.class_id_by_name("javax/net/ssl/HttpsURLConnection") else {
            return Ok(());
        };
        let Some(idx) = ctx.static_field_index_by_name(cid, "defaultSSLSocketFactory") else {
            return Ok(());
        };
        ctx.set_static_field(cid, idx, Value::Object(Some(factory)));
        Ok(())
    }
    r.register(
        hurl,
        "setDefaultSSLSocketFactory",
        "(Ljavax/net/ssl/SSLSocketFactory;)V",
        |ctx, args| {
            if let Some(Value::Object(Some(f))) = args.first() {
                capture_huc_client_identity(ctx, *f, None)?;
                publish_default_ssl_socket_factory(ctx, *f)?;            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        hurl,
        "setSSLSocketFactory",
        "(Ljavax/net/ssl/SSLSocketFactory;)V",
        |ctx, args| {
            let connection = obj_arg(args, 0)?;
            // Real JDK: `if (sf == null) throw new IllegalArgumentException`.
            let factory = match args.get(1) {
                Some(Value::Object(Some(f))) => *f,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "no SSLSocketFactory specified".to_string(),
                    }
                    .into());
                }
            };
            // gc-common w20-e: the capture resolves the factory's context and
            // can run Java (`huc_capture_for_context` takes the context by
            // `&mut` for that reason); the connection and the factory were
            // stored below through their entry addresses. Pinned across it.
            let conn_pin = ctx.pin_native_root(connection);
            let factory_pin = ctx.pin_native_root(factory);
            capture_huc_client_identity(ctx, factory, Some(connection))?;
            let connection = ctx.read_native_pin(conn_pin, connection);
            let factory = ctx.read_native_pin(factory_pin, factory);
            ctx.unpin_native_roots(conn_pin);
            // FIX (huc-per-connection-ssf-readback): this setter used to
            // capture the connection's client identity and then DROP the
            // factory object, so `getSSLSocketFactory()` could not read back
            // what was just installed (the JDK's documented round trip) and
            // `huc_client_tls_restrictions` could not find an instance-scoped
            // factory's cipher/protocol restrictions either — an instance
            // `setSSLSocketFactory` was, in effect, a no-op beyond the
            // identity capture.
            //
            // Store it in the REAL JDK instance field, exactly as
            // `setHostnameVerifier` below stores into `hostnameVerifier`:
            // an ordinary object field is already a GC root and is already
            // remapped by the moving collector, so this needs no new
            // `ObjectRef`-holding side table (the earlier note here claiming a
            // GC-rooted per-connection table was required was wrong about the
            // mechanism — the `hostnameVerifier` precedent in this same file
            // is the counter-example). It also keeps the setter and
            // `getSSLSocketFactory` reading one location, so they cannot
            // drift apart.
            ctx.set_field_by_name(connection, "sslSocketFactory", Value::Object(Some(factory)));
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        hurl,
        "getSSLSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, args| {
            // FIX (sslsocketfactory-getdefault-aether-resolution-regression-
            // 20260804): same bare-0-field carrier bug as
            // `getDefaultSSLSocketFactory` above — see that comment.
            //
            // Precedence is the JDK's, most specific first:
            //   1. this connection's own `setSSLSocketFactory(...)`, read
            //      back out of the real `sslSocketFactory` instance field;
            //   2. whatever `setDefaultSSLSocketFactory` published;
            //   3. `SSLSocketFactory.getDefault()`.
            //
            // (1) is the round trip real JDK 21 exhibits — verified by
            // `probes/HucFactoryReadbackProbe.java`, which also pins the
            // isolation half: a SECOND connection must NOT observe the first
            // one's factory, which is what makes reading a per-connection
            // field (rather than a process-wide slot) load-bearing.
            if let Some(connection) = args.first().and_then(|v| match v {
                Value::Object(Some(c)) => Some(*c),
                _ => None,
            }) {
                if let Value::Object(Some(f)) =
                    ctx.get_field_by_name(connection, "sslSocketFactory")
                {
                    return Ok(Some(Value::Object(Some(f))));
                }
            }
            let obj = huc_default_factory_or_publish(ctx)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // STUB-REMOVAL (wave 2): the two setters were `Ok(None)` no-ops and the two
    // getters always minted a fresh default verifier, so a caller-installed
    // `HostnameVerifier` was discarded and could not even be read back. That is
    // a security-relevant drop in one direction — an application installing a
    // verifier STRICTER than RFC 6125 endpoint identification had its extra
    // check silently removed — and a plain fidelity bug in the other (a
    // permissive verifier, the usual test shape, also vanished).
    //
    // Store the verifier in the REAL JDK fields (`HttpsURLConnection
    // .defaultHostnameVerifier` static, `hostnameVerifier` instance) rather
    // than a native side table: those are ordinary GC roots, so no new
    // `ObjectRef`-holding static needs wiring into
    // `gc_scan_tls_ctx_trust_manager_roots`. Both getters read them back and
    // only fall back to the synthetic default when nothing was installed.
    //
    // The stored verifier IS consulted during a request, by
    // `http_url_connection::huc_verify_hostname` — but only where real JSSE
    // consults it: as a fallback after the built-in RFC 2818 endpoint
    // identification has already FAILED, never as an additional gate every
    // connection must pass. Read that function's doc before changing anything
    // here; the real JDK's own default verifier is a hardcoded `return false`,
    // so "just call whatever is installed" rejects every https request.
    fn hurl_default_verifier_field(
        ctx: &dyn NativeContext,
    ) -> Option<(cratonvm_types::ClassId, usize)> {
        let cid = ctx.class_id_by_name("javax/net/ssl/HttpsURLConnection")?;
        let idx = ctx.static_field_index_by_name(cid, "defaultHostnameVerifier")?;
        Some((cid, idx))
    }
    r.register(
        hurl,
        "getDefaultHostnameVerifier",
        "()Ljavax/net/ssl/HostnameVerifier;",
        |ctx, _args| {
            if let Some((cid, idx)) = hurl_default_verifier_field(ctx) {
                if let Value::Object(Some(v)) = ctx.get_static_field(cid, idx) {
                    return Ok(Some(Value::Object(Some(v))));
                }
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/HostnameVerifier", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        hurl,
        "setDefaultHostnameVerifier",
        "(Ljavax/net/ssl/HostnameVerifier;)V",
        |ctx, args| {
            // Static method: slot 0 IS the verifier, not a receiver (same
            // shape as `setDefaultSSLSocketFactory` above). Real JDK rejects
            // null with IllegalArgumentException.
            let verifier = match args.first() {
                Some(Value::Object(Some(v))) => *v,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "no default HostnameVerifier specified".to_string(),
                    }
                    .into());
                }
            };
            // Resolve the field in its own statement so the immutable
            // reborrow of `ctx` is finished before the `&mut` write below.
            let field = hurl_default_verifier_field(ctx);
            if let Some((cid, idx)) = field {
                ctx.set_static_field(cid, idx, Value::Object(Some(verifier)));            }
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        hurl,
        "setHostnameVerifier",
        "(Ljavax/net/ssl/HostnameVerifier;)V",
        |ctx, args| {
            let connection = obj_arg(args, 0)?;
            let verifier = match args.get(1) {
                Some(Value::Object(Some(v))) => *v,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "no HostnameVerifier specified".to_string(),
                    }
                    .into());
                }
            };
            ctx.set_field_by_name(
                connection,
                "hostnameVerifier",
                Value::Object(Some(verifier)),
            );
            Ok(None)
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    r.register(
        hurl,
        "getHostnameVerifier",
        "()Ljavax/net/ssl/HostnameVerifier;",
        |ctx, args| {
            if let Ok(connection) = obj_arg(args, 0) {
                if let Value::Object(Some(v)) =
                    ctx.get_field_by_name(connection, "hostnameVerifier")
                {
                    return Ok(Some(Value::Object(Some(v))));
                }
            }
            if let Some((cid, idx)) = hurl_default_verifier_field(ctx) {
                if let Value::Object(Some(v)) = ctx.get_static_field(cid, idx) {
                    return Ok(Some(Value::Object(Some(v))));
                }
            }
            let obj = try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/HostnameVerifier", 0)?;
            Ok(Some(Value::Object(Some(obj))))
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // HostnameVerifier.verify — this native is registered on the *interface*
    // `javax/net/ssl/HostnameVerifier`, so it can be reached two ways:
    //
    //   1. The VM's OWN default verifier, allocated above via
    //      `try_alloc_concurrent_synthetic(ctx, "javax/net/ssl/HostnameVerifier", 0)?`.
    //      Its runtime class is the bare interface itself (no concrete
    //      subclass). For that object we short-circuit `true`: the underlying
    //      rustls/native-tls handshake already validated the SNI hostname
    //      against the peer certificate, so the default JDK verifier is a
    //      no-op here.
    //
    //   2. An APP-SUPPLIED custom `HostnameVerifier`. Normally the interpreter
    //      dispatches `verifier.verify(host, session)` straight to the
    //      subclass's bytecode and never reaches this native. But if interface
    //      resolution lands on this interface-registered native instead of the
    //      concrete impl, hardcoding `true` would silently BYPASS the app's
    //      verifier (a security hole). So for any receiver whose concrete class
    //      is NOT the bare interface, we re-dispatch to the receiver's real
    //      `verify()` so the application's logic actually runs.
    //
    // A thread-local guard prevents unbounded recursion in the degenerate case
    // where re-dispatch resolves back to this same native for the same call.
    r.register(
        "javax/net/ssl/HostnameVerifier",
        "verify",
        "(Ljava/lang/String;Ljavax/net/ssl/SSLSession;)Z",
        |ctx, args| {
            // args[0] = receiver, args[1] = hostname String, args[2] = SSLSession.
            let receiver = match args.first() {
                Some(Value::Object(Some(r))) => *r,
                // Null/garbage receiver: nothing to dispatch to. The handshake
                // already validated the hostname, so treat as the default
                // verifier and accept.
                _ => return Ok(Some(Value::Int(1))),
            };

            // Determine whether this is the VM's own default verifier (whose
            // runtime class is the bare interface) or an app-supplied concrete
            // subclass.
            let cid = ctx.class_id_of_object(receiver);
            let concrete = ctx.class_name_of_id(cid);
            let is_default_verifier = match concrete.as_deref() {
                // Bare interface instance == our default synthetic verifier.
                Some("javax/net/ssl/HostnameVerifier") | None => true,
                _ => false,
            };

            if is_default_verifier || HOSTNAME_VERIFY_REENTRANT.with(|f| f.get()) {
                // Default verifier, OR we are already inside a re-dispatch for
                // this thread (avoid infinite recursion): accept, since rustls
                // already validated the SNI hostname.
                return Ok(Some(Value::Int(1)));
            }

            // App-supplied custom verifier: run ITS real verify() rather than
            // hardcoding true. Forward the original (host, session) arguments.
            let cls = concrete.expect("concrete verifier class name");
            let call_args = [
                Value::Object(Some(receiver)),
                args.get(1).copied().unwrap_or(Value::Object(None)),
                args.get(2).copied().unwrap_or(Value::Object(None)),
            ];
            HOSTNAME_VERIFY_REENTRANT.with(|f| f.set(true));
            let result = ctx.invoke(
                &cls,
                "verify",
                "(Ljava/lang/String;Ljavax/net/ssl/SSLSession;)Z",
                &call_args,
            );
            HOSTNAME_VERIFY_REENTRANT.with(|f| f.set(false));
            result
        }, cratonvm_native_api::NativeKind::Bridge,
    );
    // Cipher suite / peer principal accessors come from the underlying
    // SSLSession/SSLSocket registrations in phases_late; we do not duplicate
    // them here to avoid conflicting registrations.
}

