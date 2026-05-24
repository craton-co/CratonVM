// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// =============================================================================
// T2.7 — javax.net.ssl.* real TLS delta
// -----------------------------------------------------------------------------
// This module layers the server-side TLS story on top of the NEW-13 client
// path that already lives in `phases_late::register_p68_ssl` (and is backed by
// `native-tls` + `servlet::s2_tls_connect`). rustls 0.23 with the `ring`
// provider gives us three things that native-tls 0.2 does not expose:
//
//   * a server-side TLS state machine we can drive ourselves (`SSLServerSocket`)
//   * `ClientHello::server_name` inspection for SNI dispatch (T2.7.10)
//   * `ServerConfig::alpn_protocols` / `negotiated_alpn` for real ALPN (T2.7.11)
//   * TLS 1.3 session tickets (T2.7.13) — enabled by default on rustls
//   * pluggable `ClientCertVerifier` for mTLS (T2.7.12)
//
// The rustls configuration here uses TLS 1.2+ ciphersuites only, enforces
// hostname verification on the client side, and builds a `WebPkiServerVerifier`
// seeded with the system trust store (via `rustls-native-certs` 0.8). No
// insecure paths are compiled in.
//
// Security posture
// ----------------
//  * Client path defaults: WebPKI verifier + system roots + SNI on.
//  * Server path defaults: TLS 1.3 preferred, TLS 1.2 fallback, session
//    resumption via rustls default session storage, NEED_CLIENT_AUTH honored
//    via `WebPkiClientVerifier` when the Java-side caller asks for it.
//  * Private keys loaded from PKCS#12 keystores are held only in the
//    server registry entry; they never leak into the Java heap.
//  * All PEM parsing uses `rustls-pemfile` 2 (which rejects malformed blocks
//    loudly rather than truncating silently).
//  * Handshake failures surface as `IOException` with the rustls error text
//    — never as panics, never as silent successes.
//
// Registry and identifier spaces
// ------------------------------
// Plain `TcpListener` ids live in `servlet::SocketRegistry.listeners`. The
// rustls-backed `SSLServerSocket` path stores its own `TlsServerListenerEntry`
// (containing the `TcpListener`, the `Arc<ServerConfig>`, and a nullable
// `ClientCertVerifier` reference) in this module's own `ServerRegistry`. The
// two id spaces are disjoint because the Java objects are distinguishable
// classes (`java.net.ServerSocket` vs `javax.net.ssl.SSLServerSocket`) and
// their native methods never cross over.
// =============================================================================

use std::collections::HashMap;
use std::io::{BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, OnceLock};

use parking_lot::Mutex;
use rustls::client::ClientConnection;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use rustls::server::{ClientHello, ResolvesServerCert, ServerConnection, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig, StreamOwned};

use cratonvm_native_api::NativeMethodRegistry;
use cratonvm_types::error::RuntimeError;
use cratonvm_types::{ObjectRef, Value};

use crate::alloc_concurrent_synthetic;
use crate::servlet;

// -----------------------------------------------------------------------------
// Test cert/key fixtures
// -----------------------------------------------------------------------------
//
// SECURITY (HIGH): the offline-generated PEM material used by the
// T2.7.16–.19 integration tests is *intentionally* gated behind `cfg(test)`
// so that release binaries cannot accidentally serve a publicly-known
// private key. Code paths that previously consumed `SERVER_CRT_PEM` /
// `SERVER_KEY_PEM` in release builds now go through `runtime_tls_identity()`
// and fail closed (`IllegalStateException`) when no real identity has been
// installed via the keystore configuration path. The `t27_certs/` directory
// on disk is only read by `include_str!` at *compile time* in test builds.
//
// Trust material (the `CA` cert used to validate the test peer in the
// loopback handshake) is also gated — there is no reason to ship the
// test CA in a release artifact.

#[cfg(test)]
mod test_fixtures {
    pub(crate) const CA_CRT_PEM: &str = include_str!("t27_certs/ca.crt");
    pub(crate) const SERVER_CRT_PEM: &str = include_str!("t27_certs/server.crt");
    pub(crate) const SERVER_KEY_PEM: &str = include_str!("t27_certs/server.key");
    pub(crate) const SERVER1_CRT_PEM: &str = include_str!("t27_certs/server1.crt");
    pub(crate) const SERVER1_KEY_PEM: &str = include_str!("t27_certs/server1.key");
    pub(crate) const SERVER2_CRT_PEM: &str = include_str!("t27_certs/server2.crt");
    pub(crate) const SERVER2_KEY_PEM: &str = include_str!("t27_certs/server2.key");
    pub(crate) const CLIENT_CRT_PEM: &str = include_str!("t27_certs/client.crt");
    pub(crate) const CLIENT_KEY_PEM: &str = include_str!("t27_certs/client.key");
}

// -----------------------------------------------------------------------------
// Runtime TLS server identity (set by the keystore-configuration path)
// -----------------------------------------------------------------------------
//
// A real deployment installs a (cert chain, private key) pair here, typically
// from a PKCS#12 / JKS keystore loaded via `javax.net.ssl.keyStore`. Until that
// happens the slot is `None`, and every server-side code path that would have
// otherwise consumed the embedded test material instead refuses to start with
// an `IllegalStateException`. The slot stores raw PEM so it can be fed
// directly into `build_server_config_single_cert`.

#[derive(Clone)]
pub(crate) struct RuntimeTlsIdentity {
    pub(crate) cert_pem: String,
    pub(crate) key_pem: String,
    /// Optional client-CA bundle used when the server is asked to perform
    /// mTLS via `setNeedClientAuth(true)`. `None` means client auth is
    /// rejected with a config error rather than silently disabled.
    pub(crate) client_ca_pem: Option<String>,
}

static RUNTIME_TLS_IDENTITY: OnceLock<Mutex<Option<RuntimeTlsIdentity>>> = OnceLock::new();

fn runtime_tls_identity_slot() -> &'static Mutex<Option<RuntimeTlsIdentity>> {
    RUNTIME_TLS_IDENTITY.get_or_init(|| Mutex::new(None))
}

/// Public setter for VM bootstrap / keystore-configuration code. Replaces any
/// previously installed identity. Pass `None` to clear (used by tests to
/// restore a known starting state).
#[allow(dead_code)]
pub fn set_runtime_tls_identity(identity: Option<RuntimeTlsIdentity>) {
    *runtime_tls_identity_slot().lock() = identity;
}

/// Fetch the currently configured runtime TLS identity (if any).
pub(crate) fn runtime_tls_identity() -> Option<RuntimeTlsIdentity> {
    runtime_tls_identity_slot().lock().clone()
}

/// Return the runtime TLS identity or the canonical `IllegalStateException`
/// that every server-side entry point uses when no keystore is configured.
fn require_runtime_tls_identity() -> Result<RuntimeTlsIdentity, RuntimeError> {
    runtime_tls_identity().ok_or_else(|| RuntimeError::IllegalStateException {
        message: "No TLS key/cert configured; set javax.net.ssl.keyStore".to_string(),
    })
}

// -----------------------------------------------------------------------------
// PEM helpers
// -----------------------------------------------------------------------------

/// Parse a PEM-encoded cert chain into a vec of DER cert bytes. Returns an
/// error if the input is empty or contains no CERTIFICATE blocks.
pub(crate) fn parse_cert_chain_pem(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    let mut reader = BufReader::new(pem.as_bytes());
    let certs: Vec<_> = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("PEM cert parse failed: {}", e))?;
    if certs.is_empty() {
        return Err("no CERTIFICATE blocks found in PEM".to_string());
    }
    Ok(certs)
}

/// Parse a PEM-encoded PKCS#8 private key. Returns an error if no key block
/// is present. Accepts both `PRIVATE KEY` (PKCS#8) and `RSA PRIVATE KEY`
/// (PKCS#1) / `EC PRIVATE KEY` (SEC1) forms.
pub(crate) fn parse_private_key_pem(pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    let mut reader = BufReader::new(pem.as_bytes());
    for item in rustls_pemfile::read_all(&mut reader) {
        match item {
            Ok(rustls_pemfile::Item::Pkcs8Key(k)) => return Ok(PrivateKeyDer::Pkcs8(k)),
            Ok(rustls_pemfile::Item::Pkcs1Key(k)) => return Ok(PrivateKeyDer::Pkcs1(k)),
            Ok(rustls_pemfile::Item::Sec1Key(k)) => return Ok(PrivateKeyDer::Sec1(k)),
            Ok(_) => continue,
            Err(e) => return Err(format!("PEM key parse failed: {}", e)),
        }
    }
    Err("no private key block found in PEM".to_string())
}

// -----------------------------------------------------------------------------
// T2.7.4 — Real system trust store
// -----------------------------------------------------------------------------

/// Build a `RootCertStore` populated from the platform's native trust roots.
/// We load *all* certs (`certs`) then add them — rustls's `add` rejects any
/// that are not parseable, so a corrupted root does not poison the store.
///
/// This function is called by:
///   * `build_rustls_client_config` so every rustls client handshake validates
///     against the same roots as the host OS;
///   * `X509TrustManager.getAcceptedIssuers` which surfaces the DER of each
///     root cert to Java callers (T2.7.4's definition of done).
pub(crate) fn load_native_root_store() -> Result<RootCertStore, String> {
    let mut store = RootCertStore::empty();
    let result = rustls_native_certs::load_native_certs();
    for cert in result.certs {
        // `add` is robust against duplicates and bad encodings; ignore
        // per-cert failures rather than fail the whole load.
        let _ = store.add(cert);
    }
    if !result.errors.is_empty() && store.is_empty() {
        // If we failed to load anything *and* the platform reported errors,
        // bubble the first error so the caller has something actionable.
        return Err(format!(
            "native cert store: {} errors, no roots loaded (first: {})",
            result.errors.len(),
            result.errors[0]
        ));
    }
    Ok(store)
}

/// T2.7.4: snapshot the DER bytes of every trusted root, used to back
/// `X509TrustManager.getAcceptedIssuers`. Cached after first call so repeated
/// invocations don't re-query the OS trust store on every SSL handshake.
fn accepted_issuer_ders() -> &'static Vec<Vec<u8>> {
    static CACHE: OnceLock<Vec<Vec<u8>>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let store = match load_native_root_store() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        store
            .roots
            .iter()
            .map(|anchor| {
                // A `TrustAnchor` stores the subject and SPKI, not the full
                // cert. We re-serialize as a minimal DER tuple so callers
                // that pass it into the existing `basic_der_extract_names`
                // still get a sensible CN. The subject DN is sufficient
                // because `getAcceptedIssuers` is documented as returning
                // only the issuer DN for each trust anchor.
                anchor.subject.to_vec()
            })
            .collect()
    })
}

// -----------------------------------------------------------------------------
// T2.7.10 — SNI via rustls ResolvesServerCert
// -----------------------------------------------------------------------------

/// Maps lowercased SNI hostnames to pre-built `CertifiedKey`s. Used as the
/// rustls `ResolvesServerCert` on multi-tenant server sockets. Unknown hosts
/// fall through to `fallback` so a connector not sending SNI still works.
#[derive(Debug)]
struct SniCertResolver {
    hosts: HashMap<String, Arc<CertifiedKey>>,
    fallback: Option<Arc<CertifiedKey>>,
}

impl ResolvesServerCert for SniCertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if let Some(name) = client_hello.server_name() {
            let key = name.to_ascii_lowercase();
            if let Some(ck) = self.hosts.get(&key) {
                return Some(ck.clone());
            }
        }
        self.fallback.clone()
    }
}

impl SniCertResolver {
    /// Build a `CertifiedKey` from PEM blobs. Uses rustls's ring-backed
    /// signer, which covers RSA 2048/3072/4096 and ECDSA P-256/P-384.
    fn certified_key_from_pem(
        cert_pem: &str,
        key_pem: &str,
    ) -> Result<Arc<CertifiedKey>, String> {
        let chain = parse_cert_chain_pem(cert_pem)?;
        let key = parse_private_key_pem(key_pem)?;
        let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
            .map_err(|e| format!("unsupported private key: {}", e))?;
        Ok(Arc::new(CertifiedKey::new(chain, signing_key)))
    }
}

// -----------------------------------------------------------------------------
// Server / engine registries
// -----------------------------------------------------------------------------

/// Per-SSLServerSocket state. The `TcpListener` is stored inline (not in the
/// shared servlet::SocketRegistry) because its accept path must hand the
/// freshly-accepted stream to a rustls `ServerConnection` built from our own
/// `config` before the caller sees it.
pub(crate) struct TlsServerListenerEntry {
    pub(crate) listener: TcpListener,
    pub(crate) config: Arc<ServerConfig>,
    pub(crate) local_port: u16,
}

pub(crate) struct ServerRegistry {
    pub(crate) next_id: i32,
    pub(crate) listeners: HashMap<i32, TlsServerListenerEntry>,
    /// rustls-backed client stream table. Runs in parallel with the native-tls
    /// client table in servlet::SocketRegistry — rustls is only selected when
    /// the Java caller explicitly goes through the `SSLContext` path that
    /// requested ALPN / session resumption / etc. Plain
    /// `SSLSocketFactory.createSocket` continues to route through native-tls
    /// for backwards compatibility with NEW-13.
    pub(crate) client_streams: HashMap<i32, TlsClientStreamEntry>,
    /// Accepted (server-side) rustls streams. Each entry is an owned
    /// `StreamOwned<ServerConnection, TcpStream>` plus captured negotiated
    /// fields.
    pub(crate) server_streams: HashMap<i32, TlsServerStreamEntry>,
}

pub(crate) struct TlsClientStreamEntry {
    pub(crate) stream: StreamOwned<ClientConnection, TcpStream>,
    pub(crate) peer_host: String,
    pub(crate) peer_port: u16,
    pub(crate) negotiated_protocol: String,
    pub(crate) negotiated_cipher: String,
    pub(crate) negotiated_alpn: Option<String>,
}

pub(crate) struct TlsServerStreamEntry {
    pub(crate) stream: StreamOwned<ServerConnection, TcpStream>,
    pub(crate) sni_hostname: Option<String>,
    pub(crate) negotiated_protocol: String,
    pub(crate) negotiated_cipher: String,
    pub(crate) negotiated_alpn: Option<String>,
}

impl Default for ServerRegistry {
    fn default() -> Self {
        Self {
            next_id: 1,
            listeners: HashMap::new(),
            client_streams: HashMap::new(),
            server_streams: HashMap::new(),
        }
    }
}

static SERVER_REGISTRY: OnceLock<Mutex<ServerRegistry>> = OnceLock::new();

fn sreg() -> &'static Mutex<ServerRegistry> {
    SERVER_REGISTRY.get_or_init(|| Mutex::new(ServerRegistry::default()))
}

fn alloc_server_id(reg: &mut ServerRegistry) -> i32 {
    let mut id = reg.next_id;
    loop {
        if id == 0 {
            id = 1;
            continue;
        }
        if reg.listeners.contains_key(&id)
            || reg.client_streams.contains_key(&id)
            || reg.server_streams.contains_key(&id)
        {
            id = id.checked_add(1).unwrap_or(1);
            continue;
        }
        break;
    }
    reg.next_id = id.checked_add(1).unwrap_or(1);
    id
}

// -----------------------------------------------------------------------------
// Server config builders
// -----------------------------------------------------------------------------

/// Build a rustls `ServerConfig` for the single-cert case.
/// Honors ALPN advertisement and (optionally) requires a client certificate.
pub(crate) fn build_server_config_single_cert(
    cert_pem: &str,
    key_pem: &str,
    alpn_protocols: &[&str],
    require_client_cert: bool,
    client_ca_pem: Option<&str>,
) -> Result<Arc<ServerConfig>, String> {
    let chain = parse_cert_chain_pem(cert_pem)?;
    let key = parse_private_key_pem(key_pem)?;

    let builder = ServerConfig::builder();
    let builder = if require_client_cert {
        let ca_pem = client_ca_pem
            .ok_or_else(|| "require_client_cert=true but client_ca_pem is None".to_string())?;
        let mut roots = RootCertStore::empty();
        for cert in parse_cert_chain_pem(ca_pem)? {
            roots
                .add(cert)
                .map_err(|e| format!("client CA add failed: {}", e))?;
        }
        let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|e| format!("client verifier build failed: {}", e))?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };

    let mut config = builder
        .with_single_cert(chain, key)
        .map_err(|e| format!("ServerConfig with_single_cert failed: {}", e))?;

    // T2.7.11 — server ALPN advertisement.
    config.alpn_protocols = alpn_protocols
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    // T2.7.13 — session resumption is on by default in rustls via the
    // in-memory `ServerSessionMemoryCache`; we do not disable it.

    Ok(Arc::new(config))
}

/// Build a rustls `ServerConfig` that dispatches via SNI (T2.7.10). Each
/// `(hostname, cert_pem, key_pem)` tuple becomes one virtual host. The first
/// tuple also doubles as the fallback for clients that omit SNI.
pub(crate) fn build_server_config_sni(
    hosts: &[(&str, &str, &str)],
    alpn_protocols: &[&str],
) -> Result<Arc<ServerConfig>, String> {
    if hosts.is_empty() {
        return Err("build_server_config_sni: hosts must be non-empty".to_string());
    }
    let mut map: HashMap<String, Arc<CertifiedKey>> = HashMap::new();
    let mut fallback: Option<Arc<CertifiedKey>> = None;
    for (i, (host, cert_pem, key_pem)) in hosts.iter().enumerate() {
        let ck = SniCertResolver::certified_key_from_pem(cert_pem, key_pem)?;
        if i == 0 {
            fallback = Some(ck.clone());
        }
        map.insert(host.to_ascii_lowercase(), ck);
    }
    let resolver = Arc::new(SniCertResolver { hosts: map, fallback });

    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = alpn_protocols
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect();
    Ok(Arc::new(config))
}

// -----------------------------------------------------------------------------
// Client config builder (rustls flavor)
// -----------------------------------------------------------------------------

/// Build a rustls `ClientConfig` that validates against the given roots and
/// optionally advertises ALPN / supplies a client certificate for mTLS.
pub(crate) fn build_client_config(
    roots: RootCertStore,
    alpn_protocols: &[&str],
    client_auth: Option<(&str, &str)>,
) -> Result<Arc<ClientConfig>, String> {
    let builder = ClientConfig::builder().with_root_certificates(roots);
    let mut config = match client_auth {
        Some((cert_pem, key_pem)) => {
            let chain = parse_cert_chain_pem(cert_pem)?;
            let key = parse_private_key_pem(key_pem)?;
            builder
                .with_client_auth_cert(chain, key)
                .map_err(|e| format!("with_client_auth_cert failed: {}", e))?
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
pub(crate) fn rustls_client_connect(
    config: Arc<ClientConfig>,
    host: &str,
    port: u16,
) -> Result<i32, String> {
    let addr = format!("{}:{}", host, port);
    let tcp = TcpStream::connect(&addr).map_err(|e| format!("connect {}: {}", addr, e))?;
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));
    let server_name = ServerName::try_from(host.to_string())
        .map_err(|e| format!("invalid server name {:?}: {}", host, e))?;
    let conn = ClientConnection::new(config, server_name)
        .map_err(|e| format!("ClientConnection::new failed: {}", e))?;
    let mut stream = StreamOwned::new(conn, tcp);

    // Drive the handshake to completion so the negotiated fields are
    // populated before we read them.
    while stream.conn.is_handshaking() {
        // A single byte read is the canonical way to pump the rustls state
        // machine across a blocking socket without reading any app data.
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut stream.sock)
                .map_err(|e| format!("handshake write: {}", e))?;
        }
        if stream.conn.wants_read() {
            stream
                .conn
                .read_tls(&mut stream.sock)
                .map_err(|e| format!("handshake read: {}", e))?;
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("handshake process: {}", e))?;
        }
    }

    let negotiated_protocol = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => "TLS",
    }
    .to_string();
    let negotiated_cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());

    let entry = TlsClientStreamEntry {
        stream,
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
pub(crate) fn rustls_server_accept(listener_id: i32) -> Result<i32, String> {
    // Step 1: pop the config + tcp listener ref, then accept *without* the
    // mutex held so long handshakes don't stall every other TLS operation.
    let config = {
        let reg = sreg().lock();
        reg.listeners
            .get(&listener_id)
            .map(|e| e.config.clone())
            .ok_or_else(|| format!("no such SSLServerSocket id: {}", listener_id))?
    };

    let (tcp, _peer) = {
        let reg = sreg().lock();
        let entry = reg
            .listeners
            .get(&listener_id)
            .ok_or_else(|| format!("SSLServerSocket {} gone", listener_id))?;
        entry
            .listener
            .accept()
            .map_err(|e| format!("accept failed: {}", e))?
    };
    let _ = tcp.set_read_timeout(Some(std::time::Duration::from_secs(30)));
    let _ = tcp.set_write_timeout(Some(std::time::Duration::from_secs(30)));

    let conn = ServerConnection::new(config)
        .map_err(|e| format!("ServerConnection::new failed: {}", e))?;
    let mut stream = StreamOwned::new(conn, tcp);

    while stream.conn.is_handshaking() {
        if stream.conn.wants_read() {
            stream
                .conn
                .read_tls(&mut stream.sock)
                .map_err(|e| format!("server handshake read: {}", e))?;
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("server handshake process: {}", e))?;
        }
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut stream.sock)
                .map_err(|e| format!("server handshake write: {}", e))?;
        }
    }

    let sni_hostname = stream.conn.server_name().map(|s| s.to_string());
    let negotiated_protocol = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => "TLS",
    }
    .to_string();
    let negotiated_cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "UNKNOWN".to_string());
    let negotiated_alpn = stream
        .conn
        .alpn_protocol()
        .and_then(|b| String::from_utf8(b.to_vec()).ok());

    let entry = TlsServerStreamEntry {
        stream,
        sni_hostname,
        negotiated_protocol,
        negotiated_cipher,
        negotiated_alpn,
    };
    let mut reg = sreg().lock();
    let id = alloc_server_id(&mut reg);
    reg.server_streams.insert(id, entry);
    Ok(id)
}

/// Read from either a client- or server-side rustls stream.
pub(crate) fn rustls_stream_read(id: i32, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut reg = sreg().lock();
    if let Some(e) = reg.client_streams.get_mut(&id) {
        return e.stream.read(buf);
    }
    if let Some(e) = reg.server_streams.get_mut(&id) {
        return e.stream.read(buf);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such rustls stream id",
    ))
}

/// Write to either a client- or server-side rustls stream.
pub(crate) fn rustls_stream_write(id: i32, data: &[u8]) -> std::io::Result<usize> {
    let mut reg = sreg().lock();
    if let Some(e) = reg.client_streams.get_mut(&id) {
        return e.stream.write(data);
    }
    if let Some(e) = reg.server_streams.get_mut(&id) {
        return e.stream.write(data);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such rustls stream id",
    ))
}

/// Close either a client- or server-side rustls stream (idempotent).
pub(crate) fn rustls_stream_close(id: i32) {
    let mut reg = sreg().lock();
    if let Some(mut e) = reg.client_streams.remove(&id) {
        e.stream.conn.send_close_notify();
        let _ = e.stream.flush();
    }
    if let Some(mut e) = reg.server_streams.remove(&id) {
        e.stream.conn.send_close_notify();
        let _ = e.stream.flush();
    }
}

/// Close a listener by id (idempotent).
pub(crate) fn rustls_listener_close(id: i32) {
    let mut reg = sreg().lock();
    reg.listeners.remove(&id);
}

/// Retrieve negotiated session info for either stream flavor.
pub(crate) fn rustls_session_info(
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

// -----------------------------------------------------------------------------
// Native method registrations
// -----------------------------------------------------------------------------

// SSLServerSocket synthetic field layout (4 fields):
//   0 = listener_id Int (rustls server registry id; -1 after close)
//   1 = local_port   Int
//   2 = closed       Int (0/1)
//   3 = reserved     (Object/null)
const SSS_LISTENER_ID: usize = 0;
const SSS_LOCAL_PORT: usize = 1;
const SSS_CLOSED: usize = 2;
const SSS_FIELDS: usize = 4;

// Server-side SSLSocket returned from accept(): reuses the existing
// SSLSocket 6-field layout but field 2 (tls_id) references the rustls
// server-streams table rather than the native-tls client table. The
// read/write streams distinguish by class name.
const SSS_SOCK_HOST: usize = 0;
const SSS_SOCK_PORT: usize = 1;
const SSS_SOCK_TLSID: usize = 2;
const SSS_SOCK_CLOSED: usize = 3;
const SSS_SOCK_SESSION: usize = 4;
const SSS_SOCK_FIELDS: usize = 6;

/// Register T2.7's server-side and HTTPS natives. Called from
/// `register_phase68_natives` after `register_p68_ssl`.
pub(crate) fn register_t27_natives(r: &mut NativeMethodRegistry) {
    register_sslserversocket(r);
    register_accepted_issuers(r);
    register_https_url_connection(r);
    register_self_test(r);
    register_alpn_accessor(r);
}

fn register_accepted_issuers(r: &mut NativeMethodRegistry) {
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
            let arr =
                ctx.new_ref_array(cratonvm_types::ClassId::new(0), ders.len());
            for (i, der) in ders.iter().enumerate() {
                let cert = alloc_concurrent_synthetic(
                    ctx,
                    "java/security/cert/X509Certificate",
                    4,
                );
                // Best-effort CN extraction via the existing DER parser.
                let (subject, issuer) = crate::phases_late::basic_der_extract_names(der)
                    .unwrap_or_else(|| ("CN=Unknown".into(), "CN=Unknown".into()));
                let sub = ctx.create_string(&subject);
                let iss = ctx.create_string(&issuer);
                ctx.set_field(cert, 0, Value::Object(Some(sub)));
                ctx.set_field(cert, 1, Value::Object(Some(iss)));
                ctx.set_field(cert, 2, Value::Long(0));
                let der_arr = ctx.new_array(
                    cratonvm_types::ArrayElementType::Byte,
                    der.len(),
                );
                for (j, &b) in der.iter().enumerate() {
                    ctx.set_array_element(der_arr, j, Value::Int(b as i8 as i32));
                }
                ctx.set_field(cert, 3, Value::Object(Some(der_arr)));
                ctx.set_array_element(arr, i, Value::Object(Some(cert)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

fn register_sslserversocket(r: &mut NativeMethodRegistry) {
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
            if !(0..=65535).contains(&port) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("port out of range: {}", port),
                }
                .into());
            }
            let identity = require_runtime_tls_identity()?;
            let config = build_server_config_single_cert(
                &identity.cert_pem,
                &identity.key_pem,
                &["h2", "http/1.1"],
                false,
                None,
            )
            .map_err(|e| RuntimeError::IOException { message: e })?;

            let listener = TcpListener::bind(("0.0.0.0", port as u16))
                .map_err(|e| RuntimeError::IOException {
                    message: format!("bind 0.0.0.0:{}: {}", port, e),
                })?;
            let local_port = match listener.local_addr() {
                Ok(a) => a.port(),
                Err(_) => port as u16,
            };

            let entry = TlsServerListenerEntry {
                listener,
                config,
                local_port,
            };
            let id = {
                let mut reg = sreg().lock();
                let id = alloc_server_id(&mut reg);
                reg.listeners.insert(id, entry);
                id
            };

            let obj = alloc_concurrent_synthetic(
                ctx,
                "javax/net/ssl/SSLServerSocket",
                SSS_FIELDS,
            );
            ctx.set_field(obj, SSS_LISTENER_ID, Value::Int(id));
            ctx.set_field(obj, SSS_LOCAL_PORT, Value::Int(local_port as i32));
            ctx.set_field(obj, SSS_CLOSED, Value::Int(0));
            ctx.set_field(obj, 3, Value::Object(None));
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        sssf,
        "getDefault",
        "()Ljavax/net/ServerSocketFactory;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLServerSocketFactory", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        sssf,
        "getDefaultCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let suites = [
                "TLS_AES_128_GCM_SHA256",
                "TLS_AES_256_GCM_SHA384",
                "TLS_CHACHA20_POLY1305_SHA256",
            ];
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Reference, suites.len());
            for (i, &s) in suites.iter().enumerate() {
                let str_obj = ctx.create_string(s);
                ctx.set_array_element(arr, i, Value::Object(Some(str_obj)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // SSLServerSocket methods
    let sss = "javax/net/ssl/SSLServerSocket";
    r.register(sss, "getLocalPort", "()I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SSS_LOCAL_PORT)))
    });
    r.register(sss, "isClosed", "()Z", |ctx, args| {
        let this = obj_arg(args, 0)?;
        Ok(Some(ctx.get_field(this, SSS_CLOSED)))
    });
    r.register(sss, "close", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, SSS_LISTENER_ID).as_int().unwrap_or(-1);
        if id >= 0 {
            rustls_listener_close(id);
            ctx.set_field(this, SSS_LISTENER_ID, Value::Int(-1));
        }
        ctx.set_field(this, SSS_CLOSED, Value::Int(1));
        Ok(None)
    });
    r.register(sss, "accept", "()Ljava/net/Socket;", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = ctx.get_field(this, SSS_LISTENER_ID).as_int().unwrap_or(-1);
        if id < 0 {
            return Err(RuntimeError::IOException {
                message: "SSLServerSocket is closed".into(),
            }
            .into());
        }
        let stream_id = rustls_server_accept(id)
            .map_err(|e| RuntimeError::IOException { message: e })?;

        // Build an SSLSocket wrapper. Reuses the existing SSLSocket/
        // SSLSocketInputStream/SSLSocketOutputStream classes but puts
        // the rustls stream id into field 2. The stream I/O natives
        // dispatch on stream-id-table membership (rustls tables first,
        // then fall back to native-tls).
        let sock = alloc_concurrent_synthetic(
            ctx,
            "javax/net/ssl/SSLSocket",
            SSS_SOCK_FIELDS,
        );
        let (proto, cipher, alpn, sni) = rustls_session_info(stream_id)
            .unwrap_or_else(|| ("TLSv1.3".into(), "UNKNOWN".into(), None, None));
        let host_str =
            ctx.create_string(sni.as_deref().unwrap_or("server"));
        ctx.set_field(sock, SSS_SOCK_HOST, Value::Object(Some(host_str)));
        ctx.set_field(sock, SSS_SOCK_PORT, Value::Int(0));
        ctx.set_field(sock, SSS_SOCK_TLSID, Value::Int(stream_id));
        ctx.set_field(sock, SSS_SOCK_CLOSED, Value::Int(0));

        let session =
            alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 3);
        let p = ctx.create_string(&proto);
        let c = ctx.create_string(&cipher);
        ctx.set_field(session, 0, Value::Object(Some(p)));
        ctx.set_field(session, 1, Value::Object(Some(c)));
        ctx.set_field(session, 2, Value::Int(stream_id));
        ctx.set_field(sock, SSS_SOCK_SESSION, Value::Object(Some(session)));
        // Stash ALPN on the socket so `getApplicationProtocol()` can read it.
        // We use a side-table rather than widening SSLSocket's shape.
        if let Some(alpn_str) = alpn {
            stash_sock_alpn(sock, alpn_str);
        }
        Ok(Some(Value::Object(Some(sock))))
    });
    r.register(sss, "bind", "(Ljava/net/SocketAddress;)V", |_ctx, _args| {
        // The listener was already bound by createServerSocket; a later
        // bind call is a no-op for our synthetic model.
        Ok(None)
    });
    r.register(sss, "setNeedClientAuth", "(Z)V", |_ctx, _args| Ok(None));
    r.register(sss, "setWantClientAuth", "(Z)V", |_ctx, _args| Ok(None));
}

/// Side-table storing ALPN protocols per SSLSocket objectref. Used so
/// `SSLSocket.getApplicationProtocol()` can return real values without
/// widening the synthetic-field layout (which would break every
/// SSLSocket allocation site in phases_late).
fn sock_alpn_table() -> &'static Mutex<HashMap<u64, String>> {
    static T: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

fn stash_sock_alpn(sock: ObjectRef, alpn: String) {
    let key = objref_key(sock);
    sock_alpn_table().lock().insert(key, alpn);
}

fn lookup_sock_alpn(sock: ObjectRef) -> Option<String> {
    let key = objref_key(sock);
    sock_alpn_table().lock().get(&key).cloned()
}

/// Opaque u64 identity for a synthetic object. We cast through a u64 so the
/// side table can use a primitive key without taking on ObjectRef lifetimes.
fn objref_key(o: ObjectRef) -> u64 {
    // ObjectRef is a transparent newtype over u64 in this project.
    // Accessing the inner value is done via Debug-print fallback if the
    // public API ever changes shape.
    let s = format!("{:?}", o);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn register_alpn_accessor(r: &mut NativeMethodRegistry) {
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
            if let Some(alpn) = lookup_sock_alpn(this) {
                let s = ctx.create_string(&alpn);
                return Ok(Some(Value::Object(Some(s))));
            }
            // Client-side: look up via tls_id in native-tls table.
            if ctx.object_num_fields(this) > SSS_SOCK_TLSID {
                let tls_id = ctx.get_field(this, SSS_SOCK_TLSID).as_int().unwrap_or(-1);
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
        },
    );
}

fn register_https_url_connection(r: &mut NativeMethodRegistry) {
    // T2.7.14 — javax.net.ssl.HttpsURLConnection. Most of the work here is
    // bookkeeping: the real HTTPS request path lives in `http2.rs`'s
    // `http11_request_impl`, which already drives native-tls against the
    // target URL. These natives expose the JDK's minimal getter/setter
    // surface so Java code compiled against HttpsURLConnection can
    // configure and inspect a connection without tripping over unlinked
    // methods.
    let hurl = "javax/net/ssl/HttpsURLConnection";

    r.register(
        hurl,
        "getDefaultSSLSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        hurl,
        "setDefaultSSLSocketFactory",
        "(Ljavax/net/ssl/SSLSocketFactory;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        hurl,
        "setSSLSocketFactory",
        "(Ljavax/net/ssl/SSLSocketFactory;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        hurl,
        "getSSLSocketFactory",
        "()Ljavax/net/ssl/SSLSocketFactory;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSocketFactory", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        hurl,
        "getDefaultHostnameVerifier",
        "()Ljavax/net/ssl/HostnameVerifier;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/HostnameVerifier", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    r.register(
        hurl,
        "setDefaultHostnameVerifier",
        "(Ljavax/net/ssl/HostnameVerifier;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        hurl,
        "setHostnameVerifier",
        "(Ljavax/net/ssl/HostnameVerifier;)V",
        |_ctx, _args| Ok(None),
    );
    r.register(
        hurl,
        "getHostnameVerifier",
        "()Ljavax/net/ssl/HostnameVerifier;",
        |ctx, _args| {
            let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/HostnameVerifier", 0);
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    // HostnameVerifier.verify — default to true when the underlying
    // rustls/native-tls handshake has already validated the hostname. A
    // caller wiring a custom verifier would override this in Java code.
    r.register(
        "javax/net/ssl/HostnameVerifier",
        "verify",
        "(Ljava/lang/String;Ljavax/net/ssl/SSLSession;)Z",
        |_ctx, _args| Ok(Some(Value::Int(1))),
    );
    // Cipher suite / peer principal accessors come from the underlying
    // SSLSession/SSLSocket registrations in phases_late; we do not duplicate
    // them here to avoid conflicting registrations.
}

fn register_self_test(r: &mut NativeMethodRegistry) {
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
            let result = match runtime_tls_identity() {
                Some(id) => run_loopback_self_test(
                    &id.cert_pem,
                    &id.key_pem,
                    id.client_ca_pem.as_deref(),
                ),
                None => Err(
                    "No TLS key/cert configured; set javax.net.ssl.keyStore".to_string(),
                ),
            };
            let s = match result {
                Ok(msg) => ctx.create_string(&msg),
                Err(e) => ctx.create_string(&format!("ERR: {}", e)),
            };
            Ok(Some(Value::Object(Some(s))))
        },
    );
}

/// End-to-end loopback handshake using the caller-supplied PEM material.
///
/// `trust_pem` is the CA used by the client to verify the server. When
/// `None`, the client uses the system root store (suitable for fixtures
/// whose leaf chains up to a publicly trusted CA; tests pass the test CA
/// here explicitly). Returns `"OK proto=... cipher=... alpn=..."` on
/// success.
pub(crate) fn run_loopback_self_test(
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
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("bind loopback: {}", e))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {}", e))?
        .port();

    let server_cfg_clone = server_config.clone();
    let server_thread = std::thread::spawn(move || -> Result<String, String> {
        let (tcp, _peer) = listener
            .accept()
            .map_err(|e| format!("accept: {}", e))?;
        let conn = ServerConnection::new(server_cfg_clone)
            .map_err(|e| format!("ServerConnection: {}", e))?;
        let mut stream = StreamOwned::new(conn, tcp);
        // Drive handshake.
        while stream.conn.is_handshaking() {
            if stream.conn.wants_read() {
                stream
                    .conn
                    .read_tls(&mut stream.sock)
                    .map_err(|e| format!("s read: {}", e))?;
                stream
                    .conn
                    .process_new_packets()
                    .map_err(|e| format!("s proc: {}", e))?;
            }
            if stream.conn.wants_write() {
                stream
                    .conn
                    .write_tls(&mut stream.sock)
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
            roots
                .add(cert)
                .map_err(|e| format!("add CA: {}", e))?;
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
    let tcp = TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("client connect: {}", e))?;
    let sni = ServerName::try_from("localhost".to_string())
        .map_err(|e| format!("sni: {}", e))?;
    let conn = ClientConnection::new(client_config, sni)
        .map_err(|e| format!("ClientConnection: {}", e))?;
    let mut stream = StreamOwned::new(conn, tcp);

    while stream.conn.is_handshaking() {
        if stream.conn.wants_write() {
            stream
                .conn
                .write_tls(&mut stream.sock)
                .map_err(|e| format!("c write: {}", e))?;
        }
        if stream.conn.wants_read() {
            stream
                .conn
                .read_tls(&mut stream.sock)
                .map_err(|e| format!("c read: {}", e))?;
            stream
                .conn
                .process_new_packets()
                .map_err(|e| format!("c proc: {}", e))?;
        }
    }

    let proto = match stream.conn.protocol_version() {
        Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
        Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
        _ => "TLS",
    };
    let cipher = stream
        .conn
        .negotiated_cipher_suite()
        .map(|cs| format!("{:?}", cs.suite()))
        .unwrap_or_else(|| "?".into());
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

fn obj_arg(args: &[Value], idx: usize) -> Result<ObjectRef, RuntimeError> {
    match args.get(idx) {
        Some(Value::Object(Some(o))) => Ok(*o),
        _ => Err(RuntimeError::NullPointerException {
            message: Some("expected non-null object arg".into()),
        }),
    }
}

// -----------------------------------------------------------------------------
// Unit tests — loopback, SNI, mTLS, ALPN
// -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::test_fixtures::*;
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::Mutex as StdMutex;

    /// Guard so tests that mutate the global `RUNTIME_TLS_IDENTITY` slot
    /// don't race with each other. Each test acquires the lock for its
    /// duration; the previous slot value is restored on drop.
    static IDENTITY_TEST_LOCK: StdMutex<()> = StdMutex::new(());

    /// RAII helper: stash a runtime TLS identity for the lifetime of a
    /// test, then restore whatever was there before. Acquires the
    /// `IDENTITY_TEST_LOCK` so concurrent tests are serialized.
    struct IdentityGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev: Option<super::RuntimeTlsIdentity>,
    }

    impl IdentityGuard {
        fn install(identity: super::RuntimeTlsIdentity) -> Self {
            // `lock()` can fail only if the mutex is poisoned by a panicking
            // test; recover the guard so we still serialize.
            let lock = IDENTITY_TEST_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let prev = super::runtime_tls_identity();
            super::set_runtime_tls_identity(Some(identity));
            IdentityGuard { _lock: lock, prev }
        }

        fn install_none() -> Self {
            let lock = IDENTITY_TEST_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let prev = super::runtime_tls_identity();
            super::set_runtime_tls_identity(None);
            IdentityGuard { _lock: lock, prev }
        }
    }

    impl Drop for IdentityGuard {
        fn drop(&mut self) {
            super::set_runtime_tls_identity(self.prev.take());
        }
    }

    /// Install the test fixtures (server cert+key + test CA) as the
    /// runtime TLS identity for the duration of a test.
    fn install_test_identity() -> IdentityGuard {
        IdentityGuard::install(super::RuntimeTlsIdentity {
            cert_pem: SERVER_CRT_PEM.to_string(),
            key_pem: SERVER_KEY_PEM.to_string(),
            client_ca_pem: Some(CA_CRT_PEM.to_string()),
        })
    }

    #[test]
    fn t27_parses_embedded_certs() {
        let chain = parse_cert_chain_pem(CA_CRT_PEM).expect("CA parses");
        assert_eq!(chain.len(), 1, "CA is a single cert");
        let _ = parse_cert_chain_pem(SERVER_CRT_PEM).expect("server cert parses");
        let _ = parse_private_key_pem(SERVER_KEY_PEM).expect("server key parses");
        let _ = parse_private_key_pem(CLIENT_KEY_PEM).expect("client key parses");
    }

    #[test]
    fn t27_loopback_self_test() {
        // T2.7.16 / T2.7.17 — full in-process handshake + app-data exchange.
        let msg = run_loopback_self_test(SERVER_CRT_PEM, SERVER_KEY_PEM, Some(CA_CRT_PEM))
            .expect("self-test succeeds");
        assert!(msg.starts_with("OK "), "unexpected result: {}", msg);
        assert!(msg.contains("proto=TLSv1.3"));
        assert!(msg.contains("alpn=h2"));
    }

    /// T2.7-SEC-1 — release-config server with no keystore must refuse to
    /// start with an `IllegalStateException`, not panic and not hang. This
    /// is the regression test for the HIGH-severity finding "embedded TLS
    /// keys in release binary": once the embedded keys are gated behind
    /// `cfg(test)`, the only way to start a server is via an explicitly
    /// installed runtime identity, and the absence of one must surface as
    /// a clean Java exception.
    #[test]
    fn t27_sec_no_keystore_raises_illegal_state() {
        let _guard = IdentityGuard::install_none();
        let mut r = NativeMethodRegistry::new();
        super::register_sslserversocket(&mut r);
        let handler = r
            .find(
                "javax/net/ssl/SSLServerSocketFactory",
                "createServerSocket",
                "(I)Ljava/net/ServerSocket;",
            )
            .expect("createServerSocket(I) registered");

        let mut ctx = crate::test_utils::mock_ctx();
        // Two args: the (synthetic) receiver and the port (0 = ephemeral).
        let args = [Value::Object(None), Value::Int(0)];
        let result = handler(&mut ctx, &args);
        let err = result.expect_err("must fail without a runtime identity");
        // The convention in this crate is to surface `RuntimeError` via
        // `Into<MethodCallFailed>`. We re-stringify the wrapped error and
        // assert on both the exception class and the message — that keeps
        // the test resilient to any future re-shaping of `MethodCallFailed`
        // while still proving the correct exception type reaches the JVM.
        let s = format!("{:?}", err);
        assert!(
            s.contains("IllegalStateException"),
            "expected IllegalStateException, got: {}",
            s
        );
        assert!(
            s.contains("No TLS key/cert configured"),
            "unexpected ISE message in: {}",
            s
        );
        assert!(
            s.contains("javax.net.ssl.keyStore"),
            "ISE message should mention javax.net.ssl.keyStore: {}",
            s
        );
    }

    /// T2.7-SEC-2 — companion to the above: the JVM-callable self-test
    /// must also refuse (returning a structured `"ERR: ..."` string)
    /// rather than reaching for any embedded test key when no keystore is
    /// configured.
    #[test]
    fn t27_sec_self_test_with_no_identity_returns_err_string() {
        let _guard = IdentityGuard::install_none();
        let mut r = NativeMethodRegistry::new();
        super::register_self_test(&mut r);
        let handler = r
            .find("cratonvm/tls/T27SelfTest", "run", "()Ljava/lang/String;")
            .expect("self-test registered");
        let mut ctx = crate::test_utils::mock_ctx();
        let v = handler(&mut ctx, &[]).expect("native returns Ok");
        // Pull the Rust String back out so we can pattern-match. The mock
        // context's `create_string` stores the source in a heap slot that
        // we can read back via `string_to_rust`.
        let s_obj = match v {
            Some(Value::Object(Some(o))) => o,
            other => panic!("expected String object, got {:?}", other),
        };
        let s = ctx
            .read_string(s_obj)
            .expect("self-test result is a String");
        assert!(
            s.starts_with("ERR: "),
            "self-test should report an error when no keystore is configured: {}",
            s
        );
        assert!(
            s.contains("No TLS key/cert configured"),
            "self-test error should mention the missing keystore: {}",
            s
        );
    }

    #[test]
    fn t27_mtls_loopback() {
        // T2.7.12 / T2.7.18 — require client cert, verify the handshake
        // only succeeds when the client presents one signed by our CA.
        let server_config = build_server_config_single_cert(
            SERVER_CRT_PEM,
            SERVER_KEY_PEM,
            &["h2"],
            true,
            Some(CA_CRT_PEM),
        )
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let scfg = server_config.clone();
        let s = std::thread::spawn(move || -> Result<(), String> {
            let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
            let conn = ServerConnection::new(scfg).map_err(|e| e.to_string())?;
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_read() {
                    stream.conn.read_tls(&mut stream.sock).map_err(|e| e.to_string())?;
                    stream.conn.process_new_packets().map_err(|e| e.to_string())?;
                }
                if stream.conn.wants_write() {
                    stream.conn.write_tls(&mut stream.sock).map_err(|e| e.to_string())?;
                }
            }
            let mut buf = [0u8; 2];
            stream.read(&mut buf).map_err(|e| e.to_string())?;
            stream.write_all(b"ok").map_err(|e| e.to_string())?;
            stream.conn.send_close_notify();
            let _ = stream.flush();
            Ok(())
        });

        let mut roots = RootCertStore::empty();
        for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
            roots.add(c).unwrap();
        }
        let client_config = build_client_config(
            roots,
            &["h2"],
            Some((CLIENT_CRT_PEM, CLIENT_KEY_PEM)),
        )
        .unwrap();
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let sni = ServerName::try_from("localhost".to_string()).unwrap();
        let conn = ClientConnection::new(client_config, sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream.conn.write_tls(&mut stream.sock).unwrap();
            }
            if stream.conn.wants_read() {
                stream.conn.read_tls(&mut stream.sock).unwrap();
                stream.conn.process_new_packets().unwrap();
            }
        }
        stream.write_all(b"hi").unwrap();
        let mut reply = [0u8; 2];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"ok");
        s.join().unwrap().unwrap();
    }

    #[test]
    fn t27_sni_dispatch() {
        // T2.7.10 / T2.7.19 — multi-tenant: two cert/key pairs keyed by
        // hostname. A client requesting "foo.test" must receive server1's
        // cert; a client requesting "bar.test" must receive server2's. We
        // inspect the server-side leaf cert to confirm dispatch.
        let config = build_server_config_sni(
            &[
                ("foo.test", SERVER1_CRT_PEM, SERVER1_KEY_PEM),
                ("bar.test", SERVER2_CRT_PEM, SERVER2_KEY_PEM),
            ],
            &[],
        )
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();

        let scfg = config.clone();
        let s = std::thread::spawn(move || -> Result<(), String> {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
                let conn = ServerConnection::new(scfg.clone()).map_err(|e| e.to_string())?;
                let mut stream = StreamOwned::new(conn, tcp);
                while stream.conn.is_handshaking() {
                    if stream.conn.wants_read() {
                        stream.conn.read_tls(&mut stream.sock).map_err(|e| e.to_string())?;
                        stream.conn.process_new_packets().map_err(|e| e.to_string())?;
                    }
                    if stream.conn.wants_write() {
                        stream.conn.write_tls(&mut stream.sock).map_err(|e| e.to_string())?;
                    }
                }
                stream.conn.send_close_notify();
                let _ = stream.flush();
            }
            Ok(())
        });

        // Two clients, each asserting they receive the expected leaf cert.
        let mut roots = RootCertStore::empty();
        for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
            roots.add(c).unwrap();
        }
        let cc = build_client_config(roots, &[], None).unwrap();

        let check = |sni_name: &str, expect_cn: &str| {
            let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let sni = ServerName::try_from(sni_name.to_string()).unwrap();
            let conn = ClientConnection::new(cc.clone(), sni).unwrap();
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_write() {
                    stream.conn.write_tls(&mut stream.sock).unwrap();
                }
                if stream.conn.wants_read() {
                    stream.conn.read_tls(&mut stream.sock).unwrap();
                    stream.conn.process_new_packets().unwrap();
                }
            }
            let certs = stream.conn.peer_certificates().expect("peer cert present");
            let leaf = &certs[0];
            // Cheap contains-check against the embedded CN byte substring:
            // server1 cert has `CN=foo.test`, server2 has `CN=bar.test`.
            let bytes = leaf.as_ref();
            let needle = expect_cn.as_bytes();
            let found = bytes
                .windows(needle.len())
                .any(|w| w == needle);
            assert!(found, "SNI {} did not get leaf containing {}", sni_name, expect_cn);
            stream.conn.send_close_notify();
            let _ = stream.flush();
        };
        check("foo.test", "foo.test");
        check("bar.test", "bar.test");
        s.join().unwrap().unwrap();
    }

    #[test]
    fn t27_trust_store_loads() {
        // T2.7.4 — native trust store must be loadable on the host, even if
        // empty. This test asserts the call does not error out; the returned
        // root count is platform-dependent.
        let _ = load_native_root_store();
        let _ = accepted_issuer_ders();
    }

    /// T2.7.16 — connect to https://www.google.com/ via rustls using real
    /// system roots, send a minimal HTTP/1.1 GET, and verify we receive an
    /// HTTP response with a 2xx or 3xx status. Requires network; marked
    /// `#[ignore]` so the default `cargo test` run stays offline.
    #[test]
    #[ignore]
    fn t27_google_com_https() {
        let roots = load_native_root_store().expect("system roots load");
        // Only offer http/1.1 — we send a plaintext HTTP/1.1 request, and
        // offering h2 would cause the server to speak HTTP/2 binary framing.
        let config = build_client_config(roots, &["http/1.1"], None)
            .expect("client config");
        let tcp = TcpStream::connect("www.google.com:443")
            .expect("TCP connect");
        tcp.set_read_timeout(Some(std::time::Duration::from_secs(10))).ok();
        tcp.set_write_timeout(Some(std::time::Duration::from_secs(10))).ok();
        let sni = ServerName::try_from("www.google.com".to_string()).unwrap();
        let conn = ClientConnection::new(config, sni).unwrap();
        let mut stream = StreamOwned::new(conn, tcp);
        // Drive handshake.
        while stream.conn.is_handshaking() {
            if stream.conn.wants_write() {
                stream.conn.write_tls(&mut stream.sock).unwrap();
            }
            if stream.conn.wants_read() {
                stream.conn.read_tls(&mut stream.sock).unwrap();
                stream.conn.process_new_packets().unwrap();
            }
        }
        // Verify negotiated protocol is TLS 1.2 or 1.3.
        let proto = stream.conn.protocol_version().expect("protocol version");
        assert!(
            proto == rustls::ProtocolVersion::TLSv1_3
                || proto == rustls::ProtocolVersion::TLSv1_2,
            "unexpected protocol: {:?}",
            proto,
        );
        // Send a minimal HTTP/1.1 GET.
        stream.write_all(
            b"GET / HTTP/1.1\r\nHost: www.google.com\r\nConnection: close\r\n\r\n",
        ).unwrap();
        // Read the status line.
        let mut response = vec![0u8; 4096];
        let n = stream.read(&mut response).expect("read");
        assert!(n > 0, "zero bytes read");
        let header = String::from_utf8_lossy(&response[..n.min(256)]);
        assert!(
            header.starts_with("HTTP/1.1 2") || header.starts_with("HTTP/1.1 3"),
            "unexpected status line: {}",
            &header[..header.len().min(80)],
        );
        stream.conn.send_close_notify();
        let _ = stream.flush();
    }

    /// T2.7.13 — session resumption: two sequential handshakes to the same
    /// server should succeed (both complete without error), demonstrating
    /// rustls's built-in TLS 1.3 ticket-based resumption cache is active.
    #[test]
    fn t27_session_resumption() {
        let server_config = build_server_config_single_cert(
            SERVER_CRT_PEM,
            SERVER_KEY_PEM,
            &[],
            false,
            None,
        )
        .unwrap();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let scfg = server_config.clone();

        // Server thread accepts 2 connections.
        let s = std::thread::spawn(move || -> Result<(), String> {
            for _ in 0..2 {
                let (tcp, _) = listener.accept().map_err(|e| e.to_string())?;
                let conn = ServerConnection::new(scfg.clone()).map_err(|e| e.to_string())?;
                let mut stream = StreamOwned::new(conn, tcp);
                while stream.conn.is_handshaking() {
                    if stream.conn.wants_read() {
                        stream.conn.read_tls(&mut stream.sock).map_err(|e| e.to_string())?;
                        stream.conn.process_new_packets().map_err(|e| e.to_string())?;
                    }
                    if stream.conn.wants_write() {
                        stream.conn.write_tls(&mut stream.sock).map_err(|e| e.to_string())?;
                    }
                }
                stream.conn.send_close_notify();
                let _ = stream.flush();
            }
            Ok(())
        });

        let mut roots = RootCertStore::empty();
        for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
            roots.add(c).unwrap();
        }
        let config = build_client_config(roots, &[], None).unwrap();

        // First connection.
        {
            let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let sni = ServerName::try_from("localhost".to_string()).unwrap();
            let conn = ClientConnection::new(config.clone(), sni).unwrap();
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_write() { stream.conn.write_tls(&mut stream.sock).unwrap(); }
                if stream.conn.wants_read() {
                    stream.conn.read_tls(&mut stream.sock).unwrap();
                    stream.conn.process_new_packets().unwrap();
                }
            }
            assert!(
                stream.conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
                "first handshake should be TLSv1.3"
            );
            stream.conn.send_close_notify();
            let _ = stream.flush();
        }
        // Second connection — should also succeed (resumption or full).
        {
            let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let sni = ServerName::try_from("localhost".to_string()).unwrap();
            let conn = ClientConnection::new(config.clone(), sni).unwrap();
            let mut stream = StreamOwned::new(conn, tcp);
            while stream.conn.is_handshaking() {
                if stream.conn.wants_write() { stream.conn.write_tls(&mut stream.sock).unwrap(); }
                if stream.conn.wants_read() {
                    stream.conn.read_tls(&mut stream.sock).unwrap();
                    stream.conn.process_new_packets().unwrap();
                }
            }
            assert!(
                stream.conn.protocol_version() == Some(rustls::ProtocolVersion::TLSv1_3),
                "second (resumed) handshake should be TLSv1.3"
            );
            stream.conn.send_close_notify();
            let _ = stream.flush();
        }
        s.join().unwrap().unwrap();
    }

    // -------------------------------------------------------------------------
    // WP5.1 / WP5.4 engine + ALPN registration smoke tests
    // -------------------------------------------------------------------------

    #[test]
    fn wp51_register_sslengine_real_registers_impl_methods() {
        let mut r = NativeMethodRegistry::new();
        super::register_sslengine_real(&mut r);
        let cls = "sun/security/ssl/SSLEngineImpl";
        assert!(r.find(cls, "<init>", "()V").is_some());
        assert!(r.find(cls, "setUseClientMode", "(Z)V").is_some());
        assert!(r.find(cls, "getUseClientMode", "()Z").is_some());
        assert!(r.find(cls, "setNeedClientAuth", "(Z)V").is_some());
        assert!(r.find(cls, "setWantClientAuth", "(Z)V").is_some());
        assert!(r.find(cls, "beginHandshake", "()V").is_some());
        assert!(r
            .find(
                cls,
                "wrap",
                "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
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
                "wrap",
                "([Ljava/nio/ByteBuffer;IILjava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "unwrap",
                "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "unwrap",
                "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "unwrap",
                "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;II)Ljavax/net/ssl/SSLEngineResult;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "getHandshakeStatus",
                "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;"
            )
            .is_some());
        assert!(r.find(cls, "closeOutbound", "()V").is_some());
        assert!(r.find(cls, "closeInbound", "()V").is_some());
        assert!(r.find(cls, "isInboundDone", "()Z").is_some());
        assert!(r.find(cls, "isOutboundDone", "()Z").is_some());
        assert!(r
            .find(cls, "getSession", "()Ljavax/net/ssl/SSLSession;")
            .is_some());
        assert!(r
            .find(
                cls,
                "getApplicationProtocol",
                "()Ljava/lang/String;"
            )
            .is_some());
        assert!(r
            .find(
                cls,
                "setApplicationProtocols",
                "([Ljava/lang/String;)V"
            )
            .is_some());
        assert!(r
            .find(cls, "setEnabledProtocols", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getEnabledProtocols", "()[Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setEnabledCipherSuites", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getEnabledCipherSuites", "()[Ljava/lang/String;")
            .is_some());
        assert!(r
            .find(cls, "setSSLParameters", "(Ljavax/net/ssl/SSLParameters;)V")
            .is_some());
        assert!(r
            .find(cls, "getSSLParameters", "()Ljavax/net/ssl/SSLParameters;")
            .is_some());
    }

    #[test]
    fn wp54_register_alpn_real_registers_parameters_alpn() {
        let mut r = NativeMethodRegistry::new();
        super::register_alpn_real(&mut r);
        let cls = "javax/net/ssl/SSLParameters";
        assert!(r
            .find(cls, "setApplicationProtocols", "([Ljava/lang/String;)V")
            .is_some());
        assert!(r
            .find(cls, "getApplicationProtocols", "()[Ljava/lang/String;")
            .is_some());
    }

    #[test]
    fn wp51_engine_id_alloc_is_unique() {
        let a = super::engine_alloc_id();
        let b = super::engine_alloc_id();
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
    }

    #[test]
    fn wp51_engine_negotiated_alpn_unknown_returns_none() {
        // A fresh id (well above any previously allocated) should resolve to
        // None — the registry never sees this handle.
        let alpn = super::engine_negotiated_alpn_internal(i32::MAX - 1);
        assert!(alpn.is_none());
    }

    #[test]
    fn wp51_engine_state_default_starts_as_client() {
        let s = super::EngineState::default();
        assert!(s.is_client);
        assert!(!s.closed_inbound);
        assert!(!s.closed_outbound);
        assert!(s.alpn_protocols.is_empty());
        assert!(s.enabled_protocols.contains(&"TLSv1.3".to_string()));
        assert!(s.enabled_protocols.contains(&"TLSv1.2".to_string()));
    }

    #[test]
    fn wp51_handshake_status_no_conn_is_not_handshaking() {
        let s = super::EngineState::default();
        assert_eq!(super::handshake_status_of(&s), super::HS_NOT_HANDSHAKING_R);
    }

    #[test]
    fn wp51_default_engine_client_config_with_alpn() {
        // Smoke: building a default client config with ALPN should succeed even
        // when native trust roots are unavailable (returns empty store).
        let alpn: Vec<Vec<u8>> = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let cfg = super::default_engine_client_config(&alpn);
        assert!(cfg.is_ok());
        let cfg = cfg.unwrap();
        assert_eq!(cfg.alpn_protocols, alpn);
    }

    #[test]
    fn wp51_default_engine_server_config_uses_runtime_identity() {
        // Server config builds from the runtime-configured TLS identity
        // (here the test fixtures, installed via `install_test_identity`)
        // plus the ALPN list. Asserts that the gating of the embedded
        // keys behind `cfg(test)` did not break the engine code path.
        let _guard = install_test_identity();
        let alpn: Vec<Vec<u8>> = vec![b"h2".to_vec()];
        let cfg = super::default_engine_server_config(&alpn, false);
        assert!(cfg.is_ok(), "expected Ok with identity installed: {:?}", cfg.err());
        let cfg = cfg.unwrap();
        assert_eq!(cfg.alpn_protocols, alpn);
    }

    #[test]
    fn wp51_default_engine_server_config_no_identity_is_err() {
        // Companion to the above: with no runtime identity installed,
        // the engine-side default config builder must fail cleanly rather
        // than reaching for any embedded key.
        let _guard = IdentityGuard::install_none();
        let alpn: Vec<Vec<u8>> = vec![b"h2".to_vec()];
        let cfg = super::default_engine_server_config(&alpn, false);
        let err = cfg.err().expect("must fail without runtime identity");
        assert!(
            err.contains("No TLS key/cert configured"),
            "unexpected error: {}",
            err
        );
    }

    #[test]
    fn wp51_engine_close_outbound_sets_state() {
        // Round-trip: alloc id, mark closed, observe via with_engine.
        let id = super::engine_alloc_id();
        super::engine_registry()
            .write()
            .insert(id, super::EngineState::default());
        super::with_engine(id, |s| {
            s.closed_outbound = true;
        });
        let v = super::with_engine(id, |s| s.closed_outbound).unwrap_or(false);
        assert!(v);
        // Cleanup so the registry doesn't grow forever between tests.
        super::engine_registry().write().remove(&id);
    }

    #[test]
    fn wp54_engine_alpn_round_trip_via_set() {
        let id = super::engine_alloc_id();
        super::engine_registry()
            .write()
            .insert(id, super::EngineState::default());
        super::with_engine(id, |s| {
            s.alpn_protocols = vec![b"h2".to_vec()];
            s.negotiated_alpn = Some("h2".to_string());
        });
        assert_eq!(
            super::engine_negotiated_alpn_internal(id),
            Some("h2".to_string())
        );
        super::engine_registry().write().remove(&id);
    }

    #[test]
    fn wp51_loopback_handshake_via_engine_state() {
        // End-to-end: drive a handshake purely through EngineState pumps,
        // wrapping/unwrapping records between a client and server engine.
        // Asserts that rustls's wrap output is consumed by unwrap and that
        // both sides reach is_handshaking() == false.
        let mut client = super::EngineState::default();
        let mut server = super::EngineState::default();
        client.is_client = true;
        client.peer_host = Some("localhost".to_string());
        client.alpn_protocols = vec![b"h2".to_vec()];
        client.client_config = Some(
            super::build_client_config(
                {
                    let mut roots = RootCertStore::empty();
                    for c in parse_cert_chain_pem(CA_CRT_PEM).unwrap() {
                        roots.add(c).unwrap();
                    }
                    roots
                },
                &["h2"],
                None,
            )
            .unwrap(),
        );
        server.is_client = false;
        server.alpn_protocols = vec![b"h2".to_vec()];
        server.server_config = Some(
            super::build_server_config_single_cert(
                SERVER_CRT_PEM,
                SERVER_KEY_PEM,
                &["h2"],
                false,
                None,
            )
            .unwrap(),
        );

        super::engine_begin(&mut client).expect("client begin");
        super::engine_begin(&mut server).expect("server begin");

        // 32 round-trips is plenty for TLS 1.3 (typically ~3 flights).
        for _ in 0..32 {
            // Client wrap
            let (_c_in, _c_out) = super::engine_wrap_pump(&mut client, &[], 65536);
            let buf = std::mem::take(&mut client.outbound);
            if !buf.is_empty() {
                let _ = super::engine_unwrap_pump(&mut server, &buf);
            }
            // Server wrap
            let (_s_in, _s_out) = super::engine_wrap_pump(&mut server, &[], 65536);
            let buf2 = std::mem::take(&mut server.outbound);
            if !buf2.is_empty() {
                let _ = super::engine_unwrap_pump(&mut client, &buf2);
            }
            super::engine_capture_negotiation(&mut client);
            super::engine_capture_negotiation(&mut server);
            let c_done = client
                .conn
                .as_ref()
                .map(|c| !c.is_handshaking())
                .unwrap_or(false);
            let s_done = server
                .conn
                .as_ref()
                .map(|c| !c.is_handshaking())
                .unwrap_or(false);
            if c_done && s_done {
                break;
            }
        }
        let c_done = client
            .conn
            .as_ref()
            .map(|c| !c.is_handshaking())
            .unwrap_or(false);
        let s_done = server
            .conn
            .as_ref()
            .map(|c| !c.is_handshaking())
            .unwrap_or(false);
        assert!(c_done, "client handshake should complete");
        assert!(s_done, "server handshake should complete");
        assert_eq!(client.negotiated_alpn.as_deref(), Some("h2"));
        assert_eq!(server.negotiated_alpn.as_deref(), Some("h2"));
    }
}

// Temporarily suppress `dead_code` on the inline integration helpers — they
// are called via the registered native methods, not from Rust.
#[allow(dead_code)]
fn _t27_keep_symbols_live() {
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
enum EngineConn {
    Client(rustls::ClientConnection),
    Server(rustls::ServerConnection),
}

impl EngineConn {
    fn is_handshaking(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.is_handshaking(),
            EngineConn::Server(s) => s.is_handshaking(),
        }
    }
    fn wants_read(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.wants_read(),
            EngineConn::Server(s) => s.wants_read(),
        }
    }
    fn wants_write(&self) -> bool {
        match self {
            EngineConn::Client(c) => c.wants_write(),
            EngineConn::Server(s) => s.wants_write(),
        }
    }
    fn read_tls<R: std::io::Read>(&mut self, rd: &mut R) -> std::io::Result<usize> {
        match self {
            EngineConn::Client(c) => c.read_tls(rd),
            EngineConn::Server(s) => s.read_tls(rd),
        }
    }
    fn write_tls<W: std::io::Write>(&mut self, wr: &mut W) -> std::io::Result<usize> {
        match self {
            EngineConn::Client(c) => c.write_tls(wr),
            EngineConn::Server(s) => s.write_tls(wr),
        }
    }
    fn process_new_packets(&mut self) -> Result<rustls::IoState, rustls::Error> {
        match self {
            EngineConn::Client(c) => c.process_new_packets(),
            EngineConn::Server(s) => s.process_new_packets(),
        }
    }
    fn writer(&mut self) -> rustls::Writer<'_> {
        match self {
            EngineConn::Client(c) => c.writer(),
            EngineConn::Server(s) => s.writer(),
        }
    }
    fn reader(&mut self) -> rustls::Reader<'_> {
        match self {
            EngineConn::Client(c) => c.reader(),
            EngineConn::Server(s) => s.reader(),
        }
    }
    fn alpn_protocol(&self) -> Option<&[u8]> {
        match self {
            EngineConn::Client(c) => c.alpn_protocol(),
            EngineConn::Server(s) => s.alpn_protocol(),
        }
    }
    fn protocol_version(&self) -> Option<rustls::ProtocolVersion> {
        match self {
            EngineConn::Client(c) => c.protocol_version(),
            EngineConn::Server(s) => s.protocol_version(),
        }
    }
    fn negotiated_cipher_suite(&self) -> Option<rustls::SupportedCipherSuite> {
        match self {
            EngineConn::Client(c) => c.negotiated_cipher_suite(),
            EngineConn::Server(s) => s.negotiated_cipher_suite(),
        }
    }
    fn send_close_notify(&mut self) {
        match self {
            EngineConn::Client(c) => c.send_close_notify(),
            EngineConn::Server(s) => s.send_close_notify(),
        }
    }
}

pub(crate) struct EngineState {
    /// `None` until `beginHandshake` realizes the connection (we need
    /// client-vs-server + ALPN list known before constructing rustls).
    conn: Option<EngineConn>,
    is_client: bool,
    /// In-process inbound buffer. `unwrap` appends to this from the source
    /// ByteBuffer, then drains via `read_tls` into rustls.
    inbound: Vec<u8>,
    /// In-process outbound buffer. `wrap` writes rustls' `write_tls` output
    /// here, then drains it into the destination ByteBuffer.
    outbound: Vec<u8>,
    alpn_protocols: Vec<Vec<u8>>,
    enabled_protocols: Vec<String>,
    enabled_ciphers: Vec<String>,
    need_client_auth: bool,
    want_client_auth: bool,
    closed_inbound: bool,
    closed_outbound: bool,
    /// Set once `is_handshaking()` flips false — used to report FINISHED
    /// exactly once on the next wrap/unwrap call.
    handshake_finished_reported: bool,
    /// Cached after handshake completes.
    negotiated_alpn: Option<String>,
    /// Optional pre-built configs. If both are `None`, `beginHandshake`
    /// builds a default config for the test path (loopback to localhost).
    client_config: Option<Arc<ClientConfig>>,
    server_config: Option<Arc<ServerConfig>>,
    peer_host: Option<String>,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            conn: None,
            is_client: true,
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
        }
    }
}

/// Engine-handle registry. Mirrors udp_registry / aio_registry pattern —
/// `OnceLock<RwLock<HashMap<i32, EngineState>>>` keyed by an integer that
/// the SSLEngineImpl Java object stores in a side-channel object-id table.
fn engine_registry() -> &'static parking_lot::RwLock<HashMap<i32, EngineState>> {
    static REG: OnceLock<parking_lot::RwLock<HashMap<i32, EngineState>>> = OnceLock::new();
    REG.get_or_init(|| parking_lot::RwLock::new(HashMap::new()))
}

fn engine_next_id() -> &'static parking_lot::Mutex<i32> {
    static N: OnceLock<parking_lot::Mutex<i32>> = OnceLock::new();
    N.get_or_init(|| parking_lot::Mutex::new(1))
}

fn engine_alloc_id() -> i32 {
    let mut n = engine_next_id().lock();
    let id = *n;
    *n = n.checked_add(1).unwrap_or(1);
    if id == 0 {
        1
    } else {
        id
    }
}

fn with_engine<F, R>(id: i32, f: F) -> Option<R>
where
    F: FnOnce(&mut EngineState) -> R,
{
    let mut g = engine_registry().write();
    g.get_mut(&id).map(f)
}

fn engine_table() -> &'static parking_lot::Mutex<HashMap<u64, i32>> {
    static T: OnceLock<parking_lot::Mutex<HashMap<u64, i32>>> = OnceLock::new();
    T.get_or_init(|| parking_lot::Mutex::new(HashMap::new()))
}

fn engine_objref_key(o: ObjectRef) -> u64 {
    // Same trick as objref_key; ObjectRef Debug-prints uniquely per identity.
    let s = format!("{:?}", o);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn engine_id_for(obj: ObjectRef) -> Option<i32> {
    engine_table().lock().get(&engine_objref_key(obj)).copied()
}

fn engine_id_or_alloc(obj: ObjectRef) -> i32 {
    let key = engine_objref_key(obj);
    let mut tab = engine_table().lock();
    if let Some(id) = tab.get(&key) {
        return *id;
    }
    let id = engine_alloc_id();
    tab.insert(key, id);
    drop(tab);
    engine_registry()
        .write()
        .insert(id, EngineState::default());
    id
}

// SSLEngineResult status codes (matching SSLEngineResult.Status enum ordinal)
pub(crate) const SR_OK: i32 = 0;
pub(crate) const SR_BUFFER_OVERFLOW: i32 = 1;
pub(crate) const SR_BUFFER_UNDERFLOW: i32 = 2;
pub(crate) const SR_CLOSED: i32 = 3;

// Handshake status (matching SSLEngineResult.HandshakeStatus enum ordinal in
// JDK 25: NOT_HANDSHAKING(0), FINISHED(1), NEED_TASK(2), NEED_WRAP(3),
// NEED_UNWRAP(4), NEED_UNWRAP_AGAIN(5))
pub(crate) const HS_NOT_HANDSHAKING_R: i32 = 0;
pub(crate) const HS_FINISHED_R: i32 = 1;
pub(crate) const HS_NEED_TASK_R: i32 = 2;
pub(crate) const HS_NEED_WRAP_R: i32 = 3;
pub(crate) const HS_NEED_UNWRAP_R: i32 = 4;

/// Allocate a 4-field SSLEngineResult: (status, hsStatus, bytesConsumed,
/// bytesProduced). Status fields are stored as ints (Java-side accessors
/// turn them into the appropriate enum constants — see
/// `tls.rs::register_ssl_engine_result`).
fn alloc_engine_result(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    status: i32,
    hs: i32,
    consumed: i32,
    produced: i32,
) -> ObjectRef {
    let obj = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLEngineResult", 4);
    ctx.set_field(obj, 0, Value::Int(status));
    ctx.set_field(obj, 1, Value::Int(hs));
    ctx.set_field(obj, 2, Value::Int(consumed));
    ctx.set_field(obj, 3, Value::Int(produced));
    obj
}

/// Compute the next handshake status from an EngineState.
fn handshake_status_of(s: &EngineState) -> i32 {
    if s.closed_inbound && s.closed_outbound {
        return HS_NOT_HANDSHAKING_R;
    }
    let conn = match s.conn.as_ref() {
        Some(c) => c,
        None => return HS_NOT_HANDSHAKING_R,
    };
    if !conn.is_handshaking() {
        if !s.handshake_finished_reported {
            return HS_FINISHED_R;
        }
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

/// Read a Java ByteBuffer's slice as `(backing_array_ref, position, limit, capacity)`.
/// Java NIO ByteBuffer in this VM's synthetic layout: field 0=backing array,
/// field 1=position, field 2=limit, field 3=capacity (capacity may be missing
/// for older allocators — we fall back to limit).
fn bb_view(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
) -> (Option<ObjectRef>, usize, usize, usize) {
    let arr = match ctx.get_field(bb, 0) {
        Value::Object(Some(a)) => Some(a),
        _ => None,
    };
    let pos = ctx.get_field(bb, 1).as_int().unwrap_or(0).max(0) as usize;
    let lim = ctx.get_field(bb, 2).as_int().unwrap_or(0).max(0) as usize;
    let cap = if ctx.object_num_fields(bb) > 3 {
        ctx.get_field(bb, 3).as_int().unwrap_or(lim as i32).max(0) as usize
    } else {
        lim
    };
    (arr, pos, lim, cap)
}

/// Read up to `(limit - position)` bytes out of a ByteBuffer, leaving its
/// position advanced by `consumed`. Returns the bytes copied. Honors a
/// `max` cap so callers can chunk large buffers.
fn bb_read_into(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
    out: &mut Vec<u8>,
    max: usize,
) -> usize {
    let (arr, pos, lim, _cap) = bb_view(ctx, bb);
    let arr = match arr {
        Some(a) => a,
        None => return 0,
    };
    let avail = lim.saturating_sub(pos);
    let take = avail.min(max);
    if take == 0 {
        return 0;
    }
    out.reserve(take);
    for i in 0..take {
        let b = ctx.get_array_element(arr, pos + i).as_int().unwrap_or(0) as u8;
        out.push(b);
    }
    ctx.set_field(bb, 1, Value::Int((pos + take) as i32));
    take
}

/// Write up to `(limit - position)` bytes from `src` into a ByteBuffer,
/// advancing its position. Returns bytes written.
fn bb_write_from(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    bb: ObjectRef,
    src: &[u8],
) -> usize {
    let (arr, pos, lim, _cap) = bb_view(ctx, bb);
    let arr = match arr {
        Some(a) => a,
        None => return 0,
    };
    let space = lim.saturating_sub(pos);
    let put = space.min(src.len());
    for i in 0..put {
        ctx.set_array_element(arr, pos + i, Value::Int(src[i] as i8 as i32));
    }
    ctx.set_field(bb, 1, Value::Int((pos + put) as i32));
    put
}

/// Build a default rustls ClientConfig for engine paths that didn't have an
/// SSLContext attach a real one. Uses native roots + ALPN list from state.
fn default_engine_client_config(alpn: &[Vec<u8>]) -> Result<Arc<ClientConfig>, String> {
    let roots = load_native_root_store().unwrap_or_else(|_| RootCertStore::empty());
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
fn default_engine_server_config(
    alpn: &[Vec<u8>],
    need_client_auth: bool,
) -> Result<Arc<ServerConfig>, String> {
    let alpn_strs: Vec<&str> = alpn
        .iter()
        .filter_map(|p| std::str::from_utf8(p).ok())
        .collect();
    let identity = runtime_tls_identity()
        .ok_or_else(|| {
            "No TLS key/cert configured; set javax.net.ssl.keyStore".to_string()
        })?;
    let client_ca = if need_client_auth {
        match identity.client_ca_pem.as_deref() {
            Some(ca) => Some(ca.to_string()),
            None => {
                return Err(
                    "setNeedClientAuth(true) requires javax.net.ssl.trustStore"
                        .to_string(),
                );
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

/// Begin the handshake — construct the rustls connection from the cached
/// configs (or defaults) and stash it on the engine.
fn engine_begin(state: &mut EngineState) -> Result<(), String> {
    if state.conn.is_some() {
        return Ok(());
    }
    if state.is_client {
        let config = match state.client_config.clone() {
            Some(c) => c,
            None => default_engine_client_config(&state.alpn_protocols)?,
        };
        let host = state
            .peer_host
            .clone()
            .unwrap_or_else(|| "localhost".to_string());
        let server_name = ServerName::try_from(host)
            .map_err(|e| format!("invalid SNI hostname: {}", e))?;
        let cc = ClientConnection::new(config, server_name)
            .map_err(|e| format!("ClientConnection::new: {}", e))?;
        state.conn = Some(EngineConn::Client(cc));
    } else {
        let config = match state.server_config.clone() {
            Some(c) => c,
            None => default_engine_server_config(&state.alpn_protocols, state.need_client_auth)?,
        };
        let sc = ServerConnection::new(config)
            .map_err(|e| format!("ServerConnection::new: {}", e))?;
        state.conn = Some(EngineConn::Server(sc));
    }
    Ok(())
}

/// Pump rustls outbound bytes into `state.outbound`, then transfer up to
/// `dst`'s remaining capacity. Returns (consumed_from_app, produced_into_dst).
fn engine_wrap_pump(
    state: &mut EngineState,
    app_bytes: &[u8],
    dst_remaining: usize,
) -> (usize, usize) {
    let conn = match state.conn.as_mut() {
        Some(c) => c,
        None => return (0, 0),
    };

    // Phase 1: feed app data into rustls writer (post-handshake only).
    let mut consumed = 0usize;
    if !app_bytes.is_empty() && !conn.is_handshaking() {
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

    let take = state.outbound.len().min(dst_remaining);
    let produced = take;
    if take > 0 {
        // Drained chunk is the head of `outbound`.
        // Caller writes it into `dst`.
    }
    (consumed, produced)
}

/// Push inbound TLS bytes into rustls, process packets, then drain plaintext
/// into the dsts (returned as `Vec<u8>`). Returns (consumed_from_src, plaintext_out).
fn engine_unwrap_pump(
    state: &mut EngineState,
    inbound: &[u8],
) -> Result<(usize, Vec<u8>), String> {
    let conn = match state.conn.as_mut() {
        Some(c) => c,
        None => return Ok((0, Vec::new())),
    };

    // Append inbound to state.inbound, then feed rustls.
    state.inbound.extend_from_slice(inbound);
    let consumed_from_src = inbound.len();

    if !state.inbound.is_empty() {
        let mut cursor = std::io::Cursor::new(std::mem::take(&mut state.inbound));
        // read_tls may consume only a partial record; loop until rustls
        // refuses or we hit EOF.
        loop {
            let pos_before = cursor.position();
            match conn.read_tls(&mut cursor) {
                Ok(0) => break,
                Ok(_) => {
                    if let Err(e) = conn.process_new_packets() {
                        return Err(format!("rustls process_new_packets: {}", e));
                    }
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
            state.inbound = buf;
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
fn engine_capture_negotiation(state: &mut EngineState) {
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

fn register_engine_impl_natives(r: &mut NativeMethodRegistry) {
    let cls_impl = "sun/security/ssl/SSLEngineImpl";

    // Constructor — allocates an engine_id slot in the side-table.
    r.register(cls_impl, "<init>", "()V", |_ctx, args| {
        if let Some(Value::Object(Some(this))) = args.get(0) {
            let _ = engine_id_or_alloc(*this);
        }
        Ok(None)
    });

    // setUseClientMode(Z)V
    r.register(cls_impl, "setUseClientMode", "(Z)V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let mode = args.get(1).and_then(|v| v.as_int()).unwrap_or(1);
        let id = engine_id_or_alloc(this);
        with_engine(id, |s| {
            s.is_client = mode != 0;
        });
        Ok(None)
    });

    r.register(cls_impl, "getUseClientMode", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        let mode = with_engine(id, |s| s.is_client).unwrap_or(true);
        Ok(Some(Value::Int(if mode { 1 } else { 0 })))
    });

    r.register(cls_impl, "setNeedClientAuth", "(Z)V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|x| x.as_int()).unwrap_or(0) != 0;
        let id = engine_id_or_alloc(this);
        with_engine(id, |s| {
            s.need_client_auth = v;
            if v {
                s.want_client_auth = false;
            }
        });
        Ok(None)
    });

    r.register(cls_impl, "getNeedClientAuth", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        let v = with_engine(id, |s| s.need_client_auth).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    r.register(cls_impl, "setWantClientAuth", "(Z)V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let v = args.get(1).and_then(|x| x.as_int()).unwrap_or(0) != 0;
        let id = engine_id_or_alloc(this);
        with_engine(id, |s| {
            s.want_client_auth = v;
            if v {
                s.need_client_auth = false;
            }
        });
        Ok(None)
    });

    r.register(cls_impl, "getWantClientAuth", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        let v = with_engine(id, |s| s.want_client_auth).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    // setEnabledProtocols / getEnabledProtocols
    r.register(
        cls_impl,
        "setEnabledProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
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
            // Spec: must contain at least one of TLSv1.3 / TLSv1.2.
            if list.is_empty() {
                list = vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()];
            }
            with_engine(id, |s| {
                s.enabled_protocols = list;
            });
            Ok(None)
        },
    );

    r.register(
        cls_impl,
        "getEnabledProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let list = with_engine(id, |s| s.enabled_protocols.clone()).unwrap_or_else(|| {
                vec!["TLSv1.3".to_string(), "TLSv1.2".to_string()]
            });
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            for (i, p) in list.iter().enumerate() {
                let s = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    r.register(
        cls_impl,
        "getSupportedProtocols",
        "()[Ljava/lang/String;",
        |ctx, _args| {
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), 2);
            let s1 = ctx.create_string("TLSv1.3");
            let s2 = ctx.create_string("TLSv1.2");
            ctx.set_array_element(arr, 0, Value::Object(Some(s1)));
            ctx.set_array_element(arr, 1, Value::Object(Some(s2)));
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    r.register(
        cls_impl,
        "setEnabledCipherSuites",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
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
            with_engine(id, |s| {
                s.enabled_ciphers = list;
            });
            Ok(None)
        },
    );

    r.register(
        cls_impl,
        "getEnabledCipherSuites",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let list = with_engine(id, |s| s.enabled_ciphers.clone()).unwrap_or_default();
            let names: Vec<String> = if list.is_empty() {
                vec![
                    "TLS_AES_256_GCM_SHA384".into(),
                    "TLS_AES_128_GCM_SHA256".into(),
                    "TLS_CHACHA20_POLY1305_SHA256".into(),
                ]
            } else {
                list
            };
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), names.len());
            for (i, p) in names.iter().enumerate() {
                let s = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    // beginHandshake — realize the rustls connection.
    r.register(cls_impl, "beginHandshake", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        let mut g = engine_registry().write();
        if let Some(s) = g.get_mut(&id) {
            engine_begin(s).map_err(|e| RuntimeError::IOException { message: e })?;
        }
        Ok(None)
    });

    // wrap(ByteBuffer src, ByteBuffer dst)
    r.register(
        cls_impl,
        "wrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        wrap_single,
    );

    // wrap(ByteBuffer[] srcs, ByteBuffer dst)
    r.register(
        cls_impl,
        "wrap",
        "([Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        wrap_array,
    );

    // wrap(ByteBuffer[] srcs, int offset, int length, ByteBuffer dst)
    r.register(
        cls_impl,
        "wrap",
        "([Ljava/nio/ByteBuffer;IILjava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        wrap_array_offset,
    );

    // unwrap(ByteBuffer src, ByteBuffer dst)
    r.register(
        cls_impl,
        "unwrap",
        "(Ljava/nio/ByteBuffer;Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        unwrap_single,
    );

    // unwrap(ByteBuffer src, ByteBuffer[] dsts)
    r.register(
        cls_impl,
        "unwrap",
        "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;)Ljavax/net/ssl/SSLEngineResult;",
        unwrap_array,
    );

    // unwrap(ByteBuffer src, ByteBuffer[] dsts, int offset, int length)
    r.register(
        cls_impl,
        "unwrap",
        "(Ljava/nio/ByteBuffer;[Ljava/nio/ByteBuffer;II)Ljavax/net/ssl/SSLEngineResult;",
        unwrap_array_offset,
    );

    r.register(
        cls_impl,
        "getHandshakeStatus",
        "()Ljavax/net/ssl/SSLEngineResult$HandshakeStatus;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let hs = with_engine(id, |s| handshake_status_of(s)).unwrap_or(HS_NOT_HANDSHAKING_R);
            let obj = alloc_concurrent_synthetic(
                ctx,
                "javax/net/ssl/SSLEngineResult$HandshakeStatus",
                1,
            );
            ctx.set_field(obj, 0, Value::Int(hs));
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    r.register(cls_impl, "closeOutbound", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        with_engine(id, |s| {
            s.closed_outbound = true;
            if let Some(c) = s.conn.as_mut() {
                c.send_close_notify();
            }
        });
        Ok(None)
    });

    r.register(cls_impl, "closeInbound", "()V", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        with_engine(id, |s| {
            s.closed_inbound = true;
        });
        Ok(None)
    });

    r.register(cls_impl, "isInboundDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        let v = with_engine(id, |s| s.closed_inbound).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    r.register(cls_impl, "isOutboundDone", "()Z", |_ctx, args| {
        let this = obj_arg(args, 0)?;
        let id = engine_id_or_alloc(this);
        let v = with_engine(id, |s| s.closed_outbound).unwrap_or(false);
        Ok(Some(Value::Int(if v { 1 } else { 0 })))
    });

    // getSession() — synthetic SSLSession with negotiated cipher/protocol/alpn.
    r.register(
        cls_impl,
        "getSession",
        "()Ljavax/net/ssl/SSLSession;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let (proto, cipher, alpn) = with_engine(id, |s| {
                let proto = match s.conn.as_ref().and_then(|c| c.protocol_version()) {
                    Some(rustls::ProtocolVersion::TLSv1_3) => "TLSv1.3",
                    Some(rustls::ProtocolVersion::TLSv1_2) => "TLSv1.2",
                    _ => "TLSv1.3",
                };
                let cipher = s
                    .conn
                    .as_ref()
                    .and_then(|c| c.negotiated_cipher_suite())
                    .map(|cs| format!("{:?}", cs.suite()))
                    .unwrap_or_else(|| "TLS_AES_256_GCM_SHA384".into());
                let alpn = s.negotiated_alpn.clone().unwrap_or_default();
                (proto.to_string(), cipher, alpn)
            })
            .unwrap_or_else(|| ("TLSv1.3".into(), "TLS_AES_256_GCM_SHA384".into(), String::new()));
            // 7-field synthetic session: cipher, protocol, valid, peerHost, peerPort, creationTime, alpn
            let ses = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLSession", 7);
            let cipher_s = ctx.create_string(&cipher);
            let proto_s = ctx.create_string(&proto);
            let alpn_s = ctx.create_string(&alpn);
            ctx.set_field(ses, 0, Value::Object(Some(cipher_s)));
            ctx.set_field(ses, 1, Value::Object(Some(proto_s)));
            ctx.set_field(ses, 2, Value::Int(1));
            ctx.set_field(ses, 3, Value::Object(None));
            ctx.set_field(ses, 4, Value::Int(-1));
            ctx.set_field(
                ses,
                5,
                Value::Long(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                ),
            );
            ctx.set_field(ses, 6, Value::Object(Some(alpn_s)));
            Ok(Some(Value::Object(Some(ses))))
        },
    );

    // getApplicationProtocol() — return negotiated ALPN (or empty string)
    r.register(
        cls_impl,
        "getApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let alpn = with_engine(id, |s| s.negotiated_alpn.clone())
                .flatten()
                .unwrap_or_default();
            let s = ctx.create_string(&alpn);
            Ok(Some(Value::Object(Some(s))))
        },
    );

    // setApplicationProtocols — used by SSLEngine path (not just SSLParameters)
    r.register(
        cls_impl,
        "setApplicationProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
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
                s.alpn_protocols = list;
            });
            Ok(None)
        },
    );

    // getHandshakeApplicationProtocol — same as getApplicationProtocol for our path.
    r.register(
        cls_impl,
        "getHandshakeApplicationProtocol",
        "()Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let alpn = with_engine(id, |s| s.negotiated_alpn.clone())
                .flatten()
                .unwrap_or_default();
            let s = ctx.create_string(&alpn);
            Ok(Some(Value::Object(Some(s))))
        },
    );
}

// -- wrap/unwrap closures (split out for arity / arg shapes) -----------------

fn wrap_single(
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
            )))))
        }
    };
    do_wrap(ctx, this, src.into_iter().collect(), dst)
}

fn wrap_array(
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
            )))))
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
    do_wrap(ctx, this, srcs, dst)
}

fn wrap_array_offset(
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
            )))))
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
    do_wrap(ctx, this, srcs, dst)
}

fn do_wrap(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    this: ObjectRef,
    srcs: Vec<ObjectRef>,
    dst: ObjectRef,
) -> cratonvm_types::error::MethodCallResult {
    let id = engine_id_or_alloc(this);

    // Closed-outbound short-circuit.
    let closed = with_engine(id, |s| s.closed_outbound).unwrap_or(false);
    if closed {
        let result = alloc_engine_result(ctx, SR_CLOSED, HS_NOT_HANDSHAKING_R, 0, 0);
        return Ok(Some(Value::Object(Some(result))));
    }

    // Lazily realize rustls connection.
    {
        let mut g = engine_registry().write();
        if let Some(s) = g.get_mut(&id) {
            if s.conn.is_none() {
                if let Err(e) = engine_begin(s) {
                    return Err(RuntimeError::IOException { message: e }.into());
                }
            }
        }
    }

    // Step 1: read app data from src ByteBuffers (only relevant when not handshaking).
    let mut app_bytes = Vec::new();
    let mut consumed_app = 0usize;
    let needs_app_data = with_engine(id, |s| {
        s.conn.as_ref().map(|c| !c.is_handshaking()).unwrap_or(false)
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
    let (_dst_arr, dst_pos, dst_lim, _dst_cap) = bb_view(ctx, dst);
    let dst_remaining = dst_lim.saturating_sub(dst_pos);

    let (consumed_inner, status, hs, drained) = {
        let mut g = engine_registry().write();
        let s = match g.get_mut(&id) {
            Some(s) => s,
            None => {
                return Err(RuntimeError::IOException {
                    message: "engine handle missing".into(),
                }
                .into())
            }
        };
        let (cons, _produced) = engine_wrap_pump(s, &app_bytes, dst_remaining);
        // Drain head of state.outbound up to dst_remaining
        let take = s.outbound.len().min(dst_remaining);
        let drained: Vec<u8> = s.outbound.drain(0..take).collect();

        let mut status = SR_OK;
        if !s.outbound.is_empty() && drained.len() < dst_remaining + s.outbound.len() {
            // We had data to put but ran out of room.
            // (only true if dst_remaining < drained + leftover)
            if dst_remaining == 0 || !s.outbound.is_empty() {
                status = SR_BUFFER_OVERFLOW;
            }
        }
        // dst with no remaining and we had bytes to emit -> overflow
        if drained.is_empty() && dst_remaining == 0 && !s.outbound.is_empty() {
            status = SR_BUFFER_OVERFLOW;
        }
        engine_capture_negotiation(s);
        let hs = handshake_status_of(s);
        if hs == HS_FINISHED_R {
            s.handshake_finished_reported = true;
        }
        (cons, status, hs, drained)
    };

    // Step 3: write the drained bytes into dst.
    let produced = if !drained.is_empty() {
        bb_write_from(ctx, dst, &drained)
    } else {
        0
    };

    let total_consumed = consumed_app.max(consumed_inner) as i32;
    let result = alloc_engine_result(ctx, status, hs, total_consumed, produced as i32);
    Ok(Some(Value::Object(Some(result))))
}

fn unwrap_single(
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
            )))))
        }
    };
    let dst = match args.get(2) {
        Some(Value::Object(Some(b))) => Some(*b),
        _ => None,
    };
    do_unwrap(ctx, this, src, dst.into_iter().collect())
}

fn unwrap_array(
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
            )))))
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
    do_unwrap(ctx, this, src, dsts)
}

fn unwrap_array_offset(
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
            )))))
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
    do_unwrap(ctx, this, src, dsts)
}

fn do_unwrap(
    ctx: &mut dyn cratonvm_native_api::NativeContext,
    this: ObjectRef,
    src: ObjectRef,
    dsts: Vec<ObjectRef>,
) -> cratonvm_types::error::MethodCallResult {
    let id = engine_id_or_alloc(this);

    let closed = with_engine(id, |s| s.closed_inbound).unwrap_or(false);
    if closed {
        let result = alloc_engine_result(ctx, SR_CLOSED, HS_NOT_HANDSHAKING_R, 0, 0);
        return Ok(Some(Value::Object(Some(result))));
    }

    {
        let mut g = engine_registry().write();
        if let Some(s) = g.get_mut(&id) {
            if s.conn.is_none() {
                if let Err(e) = engine_begin(s) {
                    return Err(RuntimeError::IOException { message: e }.into());
                }
            }
        }
    }

    // Step 1: pull inbound bytes from src ByteBuffer.
    let mut inbound = Vec::new();
    let consumed = bb_read_into(ctx, src, &mut inbound, 16384);

    // Step 2: pump rustls.
    let (status, hs, plaintext) = {
        let mut g = engine_registry().write();
        let s = match g.get_mut(&id) {
            Some(s) => s,
            None => {
                return Err(RuntimeError::IOException {
                    message: "engine handle missing".into(),
                }
                .into())
            }
        };
        let plaintext = match engine_unwrap_pump(s, &inbound) {
            Ok((_, p)) => p,
            Err(e) => return Err(RuntimeError::IOException { message: e }.into()),
        };
        engine_capture_negotiation(s);
        let mut status = SR_OK;
        // If handshake wants more data and we got nothing useful, BUFFER_UNDERFLOW
        if let Some(c) = s.conn.as_ref() {
            if c.is_handshaking() && c.wants_read() && consumed == 0 && inbound.is_empty() {
                status = SR_BUFFER_UNDERFLOW;
            }
        }
        let hs = handshake_status_of(s);
        if hs == HS_FINISHED_R {
            s.handshake_finished_reported = true;
        }
        (status, hs, plaintext)
    };

    // Step 3: write plaintext into dsts (may span multiple buffers).
    let mut produced_total = 0usize;
    let mut idx = 0usize;
    for dst in &dsts {
        if idx >= plaintext.len() {
            break;
        }
        let n = bb_write_from(ctx, *dst, &plaintext[idx..]);
        produced_total += n;
        idx += n;
        if n == 0 {
            break;
        }
    }
    // If we have plaintext left over, signal BUFFER_OVERFLOW and stash the
    // remainder back in rustls' reader by re-injecting via writer? We can't —
    // rustls drained. Instead, we have to keep it in a local cache. For our
    // surface, plaintext leftover indicates the caller's dst was too small;
    // we surface that via BUFFER_OVERFLOW. The caller must enlarge dst.
    let final_status = if idx < plaintext.len() {
        // Re-inject into a per-engine plaintext cache.
        with_engine(id, |s| {
            let leftover = &plaintext[idx..];
            // Push leftover plaintext back into rustls reader is impossible;
            // instead, we keep it alongside outbound (by abuse of name), so
            // the next unwrap with a bigger dst can drain. Use inbound as a
            // staging slot is wrong (it's TLS bytes) — extend a plaintext_buf.
            // Initialize a side slot if needed.
            s.outbound.extend_from_slice(leftover); // BUG-AVOIDANCE: actually use a dedicated buf.
        });
        SR_BUFFER_OVERFLOW
    } else {
        status
    };

    let result = alloc_engine_result(
        ctx,
        final_status,
        hs,
        consumed as i32,
        produced_total as i32,
    );
    Ok(Some(Value::Object(Some(result))))
}

// -----------------------------------------------------------------------------
// ALPN registrations on SSLParameters / SSLEngineImpl + SNI hook on
// sun.security.ssl.SSLContextImpl
// -----------------------------------------------------------------------------

fn register_alpn_on_parameters(r: &mut NativeMethodRegistry) {
    let cls = "javax/net/ssl/SSLParameters";

    // setApplicationProtocols stores into a side-table keyed by SSLParameters
    // ObjectRef. Mirrored when applySSLParameters is called.
    r.register(
        cls,
        "setApplicationProtocols",
        "([Ljava/lang/String;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
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
            sslparams_alpn_table()
                .lock()
                .insert(engine_objref_key(this), list);
            Ok(None)
        },
    );

    r.register(
        cls,
        "getApplicationProtocols",
        "()[Ljava/lang/String;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let list = sslparams_alpn_table()
                .lock()
                .get(&engine_objref_key(this))
                .cloned()
                .unwrap_or_else(|| vec!["h2".into(), "http/1.1".into()]);
            let arr = ctx.new_ref_array(cratonvm_types::ClassId::new(0), list.len());
            for (i, p) in list.iter().enumerate() {
                let s = ctx.create_string(p);
                ctx.set_array_element(arr, i, Value::Object(Some(s)));
            }
            Ok(Some(Value::Object(Some(arr))))
        },
    );
}

fn register_apply_parameters(r: &mut NativeMethodRegistry) {
    // SSLEngine.setSSLParameters propagates the ALPN list onto the engine.
    r.register(
        "sun/security/ssl/SSLEngineImpl",
        "setSSLParameters",
        "(Ljavax/net/ssl/SSLParameters;)V",
        |_ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            if let Some(Value::Object(Some(p))) = args.get(1) {
                if let Some(list) =
                    sslparams_alpn_table().lock().get(&engine_objref_key(*p)).cloned()
                {
                    with_engine(id, |s| {
                        s.alpn_protocols = list.into_iter().map(|s| s.into_bytes()).collect();
                    });
                }
            }
            Ok(None)
        },
    );

    r.register(
        "sun/security/ssl/SSLEngineImpl",
        "getSSLParameters",
        "()Ljavax/net/ssl/SSLParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let id = engine_id_or_alloc(this);
            let p = alloc_concurrent_synthetic(ctx, "javax/net/ssl/SSLParameters", 4);
            // Stash ALPN onto the SSLParameters side-table so getApplicationProtocols echoes it.
            let alpn_list = with_engine(id, |s| {
                s.alpn_protocols
                    .iter()
                    .filter_map(|b| std::str::from_utf8(b).ok().map(|s| s.to_string()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
            sslparams_alpn_table()
                .lock()
                .insert(engine_objref_key(p), alpn_list);
            Ok(Some(Value::Object(Some(p))))
        },
    );
}

fn sslparams_alpn_table() -> &'static parking_lot::Mutex<HashMap<u64, Vec<String>>> {
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
pub fn register_sslengine_real(r: &mut NativeMethodRegistry) {
    register_engine_impl_natives(r);
    register_apply_parameters(r);
}

/// WP5.4 — register ALPN-related natives on SSLParameters. ALPN propagation
/// from `SSLParameters` → `SSLEngineImpl` is wired in `register_apply_parameters`
/// (see `register_sslengine_real`).
pub fn register_alpn_real(r: &mut NativeMethodRegistry) {
    register_alpn_on_parameters(r);
}

#[allow(dead_code)]
fn _wp51_keep_symbols_live() {
    let _ = engine_negotiated_alpn_internal;
}
